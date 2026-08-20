use std::{
    collections::HashSet,
    error::Error,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use reqwest::{StatusCode, header};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::{
    control::protocol::{DiscoverySource, DraftCredentialSource, ProviderAuthentication, Target},
    domain::provider::normalize_provider_base_url,
    state::{StateStore, providers::ProviderInspectionRead},
};

pub const MAX_DISCOVERY_BODY_BYTES: usize = 256 * 1024;
pub const MAX_DISCOVERED_MODELS: usize = 2_048;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const REACHABILITY_TIMEOUT: Duration = Duration::from_secs(8);
const REACHABILITY_SLOW_THRESHOLD: Duration = Duration::from_secs(6);
const MAX_ANTHROPIC_DISCOVERY_PAGES: usize = 8;

const COMPATIBILITY_SUFFIXES: [&str; 9] = [
    "/api/claudecode",
    "/api/anthropic",
    "/apps/anthropic",
    "/api/coding",
    "/claudecode",
    "/anthropic",
    "/step_plan",
    "/coding",
    "/claude",
];

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InspectionCategory {
    InvalidEndpoint,
    MissingCredential,
    MissingProvider,
    StaleProviderRevision,
    AuthenticationRejected,
    EndpointUnsupported,
    RateLimited,
    UpstreamStatus,
    Timeout,
    Dns,
    Connect,
    Tls,
    Cancelled,
    MalformedResponse,
    ResponseTooLarge,
    TooManyModels,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectionFailure {
    pub category: InspectionCategory,
    pub http_status: Option<u16>,
    pub attempts: u32,
    pub elapsed_ms: u64,
    pub endpoint_origin: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ModelDiscoveryResult {
    Success {
        models: Vec<DiscoveredModel>,
        attempts: u32,
        elapsed_ms: u64,
        endpoint_origin: String,
    },
    Failure {
        failure: InspectionFailure,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "status",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ReachabilityResult {
    Reachable {
        http_status: u16,
        ttfb_ms: u64,
        checked_at_unix_ms: u64,
        retry_count: u8,
        slow: bool,
        endpoint_origin: String,
    },
    Unreachable {
        failure: InspectionFailure,
        checked_at_unix_ms: u64,
        retry_count: u8,
    },
}

pub struct ProviderInspector {
    client: reqwest::Client,
    store: Arc<StateStore>,
    discovery_timeout: Duration,
    reachability_timeout: Duration,
    slow_threshold: Duration,
}

impl ProviderInspector {
    pub fn new(store: Arc<StateStore>) -> Result<Self, InspectionCategory> {
        Self::with_timeouts(
            store,
            DISCOVERY_TIMEOUT,
            REACHABILITY_TIMEOUT,
            REACHABILITY_SLOW_THRESHOLD,
        )
    }

    #[doc(hidden)]
    pub fn with_timeouts(
        store: Arc<StateStore>,
        discovery_timeout: Duration,
        reachability_timeout: Duration,
        slow_threshold: Duration,
    ) -> Result<Self, InspectionCategory> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| InspectionCategory::Connect)?;
        Ok(Self {
            client,
            store,
            discovery_timeout,
            reachability_timeout,
            slow_threshold,
        })
    }

    pub async fn discover_models(&self, source: DiscoverySource) -> ModelDiscoveryResult {
        self.discover_models_for(Target::Codex, source).await
    }

    pub async fn discover_models_for(
        &self,
        target: Target,
        source: DiscoverySource,
    ) -> ModelDiscoveryResult {
        let started = Instant::now();
        let (base_url, credential, authentication) =
            match self.resolve_discovery_source(target, source).await {
                Ok(resolved) => resolved,
                Err(category) => {
                    return ModelDiscoveryResult::Failure {
                        failure: inspection_failure(category, None, 0, started, None),
                    };
                }
            };
        let candidates = match build_models_url_candidates(&base_url, false, None) {
            Ok(candidates) => candidates,
            Err(category) => {
                return ModelDiscoveryResult::Failure {
                    failure: inspection_failure(category, None, 0, started, None),
                };
            }
        };

        if target == Target::Claude {
            return self
                .discover_anthropic_models(candidates, credential, authentication, started)
                .await;
        }

        for (index, candidate) in candidates.iter().enumerate() {
            let attempts = index as u32 + 1;
            let url = match Url::parse(candidate) {
                Ok(url) => url,
                Err(_) => {
                    return ModelDiscoveryResult::Failure {
                        failure: inspection_failure(
                            InspectionCategory::InvalidEndpoint,
                            None,
                            attempts,
                            started,
                            None,
                        ),
                    };
                }
            };
            let endpoint_origin = url.origin().ascii_serialization();
            let request = self.client.get(url).timeout(self.discovery_timeout);
            let request = match authentication {
                ProviderAuthentication::AnthropicApiKey => {
                    request.header("x-api-key", credential.expose_secret())
                }
                ProviderAuthentication::AnthropicBearer
                | ProviderAuthentication::OpenaiBearer
                | ProviderAuthentication::CodexSubscription => {
                    request.bearer_auth(credential.expose_secret())
                }
            };
            let request = match authentication {
                ProviderAuthentication::AnthropicApiKey
                | ProviderAuthentication::AnthropicBearer => {
                    request.header("anthropic-version", "2023-06-01")
                }
                ProviderAuthentication::OpenaiBearer
                | ProviderAuthentication::CodexSubscription => request,
            };
            let response = request.send().await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    return ModelDiscoveryResult::Failure {
                        failure: inspection_failure(
                            classify_reqwest_error(&error),
                            None,
                            attempts,
                            started,
                            Some(endpoint_origin),
                        ),
                    };
                }
            };
            let status = response.status();
            if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
                if index + 1 < candidates.len() {
                    continue;
                }
                return ModelDiscoveryResult::Failure {
                    failure: inspection_failure(
                        InspectionCategory::EndpointUnsupported,
                        Some(status.as_u16()),
                        attempts,
                        started,
                        Some(endpoint_origin),
                    ),
                };
            }
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return ModelDiscoveryResult::Failure {
                    failure: inspection_failure(
                        InspectionCategory::AuthenticationRejected,
                        Some(status.as_u16()),
                        attempts,
                        started,
                        Some(endpoint_origin),
                    ),
                };
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                return ModelDiscoveryResult::Failure {
                    failure: inspection_failure(
                        InspectionCategory::RateLimited,
                        Some(status.as_u16()),
                        attempts,
                        started,
                        Some(endpoint_origin),
                    ),
                };
            }
            if !status.is_success() {
                return ModelDiscoveryResult::Failure {
                    failure: inspection_failure(
                        InspectionCategory::UpstreamStatus,
                        Some(status.as_u16()),
                        attempts,
                        started,
                        Some(endpoint_origin),
                    ),
                };
            }

            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        return ModelDiscoveryResult::Failure {
                            failure: inspection_failure(
                                classify_reqwest_error(&error),
                                Some(status.as_u16()),
                                attempts,
                                started,
                                Some(endpoint_origin),
                            ),
                        };
                    }
                };
                if body.len().saturating_add(chunk.len()) > MAX_DISCOVERY_BODY_BYTES {
                    return ModelDiscoveryResult::Failure {
                        failure: inspection_failure(
                            InspectionCategory::ResponseTooLarge,
                            Some(status.as_u16()),
                            attempts,
                            started,
                            Some(endpoint_origin),
                        ),
                    };
                }
                body.extend_from_slice(&chunk);
            }
            return match parse_models_response_for(target, &body) {
                Ok(models) => ModelDiscoveryResult::Success {
                    models,
                    attempts,
                    elapsed_ms: elapsed_ms(started),
                    endpoint_origin,
                },
                Err(category) => ModelDiscoveryResult::Failure {
                    failure: inspection_failure(
                        category,
                        Some(status.as_u16()),
                        attempts,
                        started,
                        Some(endpoint_origin),
                    ),
                },
            };
        }

        ModelDiscoveryResult::Failure {
            failure: inspection_failure(
                InspectionCategory::InvalidEndpoint,
                None,
                0,
                started,
                None,
            ),
        }
    }

    async fn discover_anthropic_models(
        &self,
        candidates: Vec<String>,
        credential: SecretString,
        authentication: ProviderAuthentication,
        started: Instant,
    ) -> ModelDiscoveryResult {
        let mut attempts = 0;
        for candidate in candidates {
            let Ok(base) = Url::parse(&candidate) else {
                return ModelDiscoveryResult::Failure {
                    failure: inspection_failure(
                        InspectionCategory::InvalidEndpoint,
                        None,
                        attempts,
                        started,
                        None,
                    ),
                };
            };
            let origin = base.origin().ascii_serialization();
            let mut after_id: Option<String> = None;
            let mut cursors = HashSet::new();
            let mut models = Vec::new();
            let mut seen = HashSet::new();
            for page in 0..MAX_ANTHROPIC_DISCOVERY_PAGES {
                let mut url = base.clone();
                {
                    let mut query = url.query_pairs_mut();
                    query.append_pair("limit", "1000");
                    if let Some(after_id) = &after_id {
                        query.append_pair("after_id", after_id);
                    }
                }
                attempts += 1;
                let request = self.client.get(url).timeout(self.discovery_timeout);
                let request = match authentication {
                    ProviderAuthentication::AnthropicApiKey => {
                        request.header("x-api-key", credential.expose_secret())
                    }
                    ProviderAuthentication::AnthropicBearer
                    | ProviderAuthentication::OpenaiBearer
                    | ProviderAuthentication::CodexSubscription => {
                        request.bearer_auth(credential.expose_secret())
                    }
                }
                .header("anthropic-version", "2023-06-01");
                let response = match request.send().await {
                    Ok(response) => response,
                    Err(error) => {
                        return ModelDiscoveryResult::Failure {
                            failure: inspection_failure(
                                classify_reqwest_error(&error),
                                None,
                                attempts,
                                started,
                                Some(origin),
                            ),
                        };
                    }
                };
                let status = response.status();
                if !status.is_success() {
                    if page == 0
                        && matches!(
                            status,
                            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                        )
                    {
                        break;
                    }
                    return ModelDiscoveryResult::Failure {
                        failure: inspection_failure(
                            status_category(status),
                            Some(status.as_u16()),
                            attempts,
                            started,
                            Some(origin),
                        ),
                    };
                }
                let body = match bounded_body(response, status, attempts, started, &origin).await {
                    Ok(body) => body,
                    Err(failure) => return ModelDiscoveryResult::Failure { failure },
                };
                let page = match parse_anthropic_page(&body) {
                    Ok(page) => page,
                    Err(category) => {
                        return ModelDiscoveryResult::Failure {
                            failure: inspection_failure(
                                category,
                                Some(status.as_u16()),
                                attempts,
                                started,
                                Some(origin),
                            ),
                        };
                    }
                };
                for model in page.models {
                    if seen.insert(model.id.clone()) {
                        models.push(model);
                        if models.len() > MAX_DISCOVERED_MODELS {
                            return ModelDiscoveryResult::Failure {
                                failure: inspection_failure(
                                    InspectionCategory::TooManyModels,
                                    Some(status.as_u16()),
                                    attempts,
                                    started,
                                    Some(origin),
                                ),
                            };
                        }
                    }
                }
                if !page.has_more {
                    models.sort_by(|left, right| left.id.cmp(&right.id));
                    return ModelDiscoveryResult::Success {
                        models,
                        attempts,
                        elapsed_ms: elapsed_ms(started),
                        endpoint_origin: origin,
                    };
                }
                let Some(cursor) = page.last_id.filter(|value| !value.trim().is_empty()) else {
                    return ModelDiscoveryResult::Failure {
                        failure: inspection_failure(
                            InspectionCategory::MalformedResponse,
                            Some(status.as_u16()),
                            attempts,
                            started,
                            Some(origin),
                        ),
                    };
                };
                if !cursors.insert(cursor.clone()) {
                    return ModelDiscoveryResult::Failure {
                        failure: inspection_failure(
                            InspectionCategory::MalformedResponse,
                            Some(status.as_u16()),
                            attempts,
                            started,
                            Some(origin),
                        ),
                    };
                }
                after_id = Some(cursor);
            }
            if after_id.is_some() {
                return ModelDiscoveryResult::Failure {
                    failure: inspection_failure(
                        InspectionCategory::TooManyModels,
                        None,
                        attempts,
                        started,
                        Some(origin),
                    ),
                };
            }
        }
        ModelDiscoveryResult::Failure {
            failure: inspection_failure(
                InspectionCategory::EndpointUnsupported,
                None,
                attempts,
                started,
                None,
            ),
        }
    }

    pub async fn check_reachability(
        &self,
        provider_id: Uuid,
        provider_revision: u64,
    ) -> ReachabilityResult {
        self.check_reachability_for(Target::Codex, provider_id, provider_revision)
            .await
    }

    pub async fn check_reachability_for(
        &self,
        target: Target,
        provider_id: Uuid,
        provider_revision: u64,
    ) -> ReachabilityResult {
        let operation_started = Instant::now();
        let snapshot = match self
            .store
            .provider_for_inspection(target, provider_id, provider_revision)
            .await
        {
            Ok(ProviderInspectionRead::Found(snapshot)) => snapshot,
            Ok(ProviderInspectionRead::Missing) => {
                return unreachable(
                    InspectionCategory::MissingProvider,
                    0,
                    operation_started,
                    None,
                    0,
                );
            }
            Ok(ProviderInspectionRead::StaleRevision) => {
                return unreachable(
                    InspectionCategory::StaleProviderRevision,
                    0,
                    operation_started,
                    None,
                    0,
                );
            }
            Err(_) => {
                return unreachable(
                    InspectionCategory::MissingProvider,
                    0,
                    operation_started,
                    None,
                    0,
                );
            }
        };
        let url = match parse_inspection_url(&snapshot.base_url, false) {
            Ok(url) => url,
            Err(category) => {
                return unreachable(category, 0, operation_started, None, 0);
            }
        };
        let endpoint_origin = url.origin().ascii_serialization();

        for retry_count in 0..=1 {
            let attempt_started = Instant::now();
            let response = self
                .client
                .get(url.clone())
                .header(header::ACCEPT, "*/*")
                .header(header::ACCEPT_ENCODING, "identity")
                .timeout(self.reachability_timeout)
                .send()
                .await;
            match response {
                Ok(response) => {
                    let ttfb = attempt_started.elapsed();
                    return ReachabilityResult::Reachable {
                        http_status: response.status().as_u16(),
                        ttfb_ms: duration_ms(ttfb),
                        checked_at_unix_ms: now_unix_ms(),
                        retry_count,
                        slow: ttfb > self.slow_threshold,
                        endpoint_origin,
                    };
                }
                Err(error) => {
                    let category = classify_reqwest_error(&error);
                    if category == InspectionCategory::Timeout && retry_count == 0 {
                        continue;
                    }
                    return unreachable(
                        category,
                        u32::from(retry_count) + 1,
                        operation_started,
                        Some(endpoint_origin),
                        retry_count,
                    );
                }
            }
        }
        unreachable(
            InspectionCategory::Timeout,
            1,
            operation_started,
            Some(endpoint_origin),
            1,
        )
    }

    async fn resolve_discovery_source(
        &self,
        target: Target,
        source: DiscoverySource,
    ) -> Result<(String, SecretString, ProviderAuthentication), InspectionCategory> {
        match source {
            DiscoverySource::Saved {
                provider_id,
                provider_revision,
            } => {
                let snapshot = self
                    .resolve_saved_provider(target, provider_id, provider_revision)
                    .await?;
                if snapshot.authentication == ProviderAuthentication::CodexSubscription {
                    return Err(InspectionCategory::EndpointUnsupported);
                }
                let credential = snapshot
                    .credential
                    .ok_or(InspectionCategory::MissingCredential)?;
                Ok((snapshot.base_url, credential, snapshot.authentication))
            }
            DiscoverySource::Draft {
                base_url,
                authentication,
                credential_source,
            } => {
                if authentication == ProviderAuthentication::CodexSubscription {
                    return Err(InspectionCategory::EndpointUnsupported);
                }
                let credential = match credential_source {
                    DraftCredentialSource::Missing => {
                        return Err(InspectionCategory::MissingCredential);
                    }
                    DraftCredentialSource::Ephemeral { value } if !value.trim().is_empty() => {
                        SecretString::from(value)
                    }
                    DraftCredentialSource::Ephemeral { .. } => {
                        return Err(InspectionCategory::MissingCredential);
                    }
                    DraftCredentialSource::Saved {
                        provider_id,
                        provider_revision,
                    } => {
                        let snapshot = self
                            .resolve_saved_provider(target, provider_id, provider_revision)
                            .await?;
                        snapshot
                            .credential
                            .ok_or(InspectionCategory::MissingCredential)?
                    }
                };
                Ok((base_url, credential, authentication))
            }
        }
    }

    async fn resolve_saved_provider(
        &self,
        target: Target,
        provider_id: Uuid,
        provider_revision: u64,
    ) -> Result<crate::state::providers::ProviderInspectionSnapshot, InspectionCategory> {
        match self
            .store
            .provider_for_inspection(target, provider_id, provider_revision)
            .await
        {
            Ok(ProviderInspectionRead::Found(snapshot)) => Ok(snapshot),
            Ok(ProviderInspectionRead::Missing) => Err(InspectionCategory::MissingProvider),
            Ok(ProviderInspectionRead::StaleRevision) => {
                Err(InspectionCategory::StaleProviderRevision)
            }
            Err(_) => Err(InspectionCategory::MissingProvider),
        }
    }
}

pub fn build_models_url_candidates(
    base_url: &str,
    is_full_url: bool,
    models_url_override: Option<&str>,
) -> Result<Vec<String>, InspectionCategory> {
    if let Some(override_url) = models_url_override.filter(|value| !value.trim().is_empty()) {
        let override_url = parse_inspection_url(override_url.trim(), true)?;
        return Ok(vec![override_url.into()]);
    }

    let mut base = parse_inspection_url(base_url.trim().trim_end_matches('/'), false)?;
    base.set_query(None);
    let path = base.path().trim_end_matches('/').to_owned();
    let mut candidates = Vec::with_capacity(3);

    if is_full_url {
        let models_path = if let Some(index) = path.find("/v1/") {
            format!("{}/models", &path[..index + 3])
        } else {
            let (root, _) = path
                .rsplit_once('/')
                .filter(|(_, segment)| !segment.is_empty())
                .ok_or(InspectionCategory::InvalidEndpoint)?;
            format!("{root}/v1/models")
        };
        push_candidate(&mut candidates, &base, &models_path);
        return Ok(candidates);
    }

    let final_segment = path.rsplit('/').next().unwrap_or_default();
    let version_segment = final_segment.strip_prefix('v').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    if version_segment {
        push_candidate(&mut candidates, &base, &format!("{path}/models"));
        if final_segment != "v1" {
            push_candidate(&mut candidates, &base, &format!("{path}/v1/models"));
        }
    } else {
        push_candidate(&mut candidates, &base, &format!("{path}/v1/models"));
    }

    if let Some(suffix) = COMPATIBILITY_SUFFIXES
        .iter()
        .find(|suffix| path.ends_with(**suffix))
    {
        let root = &path[..path.len() - suffix.len()];
        push_candidate(&mut candidates, &base, &format!("{root}/v1/models"));
        push_candidate(&mut candidates, &base, &format!("{root}/models"));
    }

    Ok(candidates)
}

pub fn parse_models_response(body: &[u8]) -> Result<Vec<DiscoveredModel>, InspectionCategory> {
    parse_models_response_with_display_name(body, "owned_by")
}

fn parse_models_response_for(
    target: Target,
    body: &[u8],
) -> Result<Vec<DiscoveredModel>, InspectionCategory> {
    parse_models_response_with_display_name(
        body,
        if target == Target::Claude {
            "display_name"
        } else {
            "owned_by"
        },
    )
}

fn parse_models_response_with_display_name(
    body: &[u8],
    display_name_field: &str,
) -> Result<Vec<DiscoveredModel>, InspectionCategory> {
    if body.len() > MAX_DISCOVERY_BODY_BYTES {
        return Err(InspectionCategory::ResponseTooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| InspectionCategory::MalformedResponse)?;
    let object = value
        .as_object()
        .ok_or(InspectionCategory::MalformedResponse)?;
    let Some(data) = object.get("data") else {
        return Ok(Vec::new());
    };
    if data.is_null() {
        return Ok(Vec::new());
    }
    let entries = data
        .as_array()
        .ok_or(InspectionCategory::MalformedResponse)?;
    if entries.len() > MAX_DISCOVERED_MODELS {
        return Err(InspectionCategory::TooManyModels);
    }

    let mut seen = HashSet::with_capacity(entries.len());
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry
            .as_object()
            .ok_or(InspectionCategory::MalformedResponse)?;
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or(InspectionCategory::MalformedResponse)?;
        let display_name = match entry.get(display_name_field) {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or(InspectionCategory::MalformedResponse)?
                    .to_owned(),
            ),
        };
        if !id.trim().is_empty() && seen.insert(id.to_owned()) {
            models.push(DiscoveredModel {
                id: id.to_owned(),
                display_name,
            });
        }
    }
    models.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(models)
}

fn parse_inspection_url(input: &str, retain_query: bool) -> Result<Url, InspectionCategory> {
    let mut url = Url::parse(input).map_err(|_| InspectionCategory::InvalidEndpoint)?;
    if url.host().is_none()
        || url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(InspectionCategory::InvalidEndpoint);
    }
    let query = retain_query
        .then(|| url.query().map(str::to_owned))
        .flatten();
    url.set_query(None);
    let normalized = normalize_provider_base_url(url.as_str())
        .map_err(|_| InspectionCategory::InvalidEndpoint)?;
    let mut normalized =
        Url::parse(&normalized).map_err(|_| InspectionCategory::InvalidEndpoint)?;
    normalized.set_query(query.as_deref());
    Ok(normalized)
}

fn push_candidate(candidates: &mut Vec<String>, base: &Url, path: &str) {
    let mut candidate = base.clone();
    candidate.set_path(if path.is_empty() { "/" } else { path });
    candidate.set_query(None);
    candidate.set_fragment(None);
    let candidate: String = candidate.into();
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
}

struct AnthropicPage {
    models: Vec<DiscoveredModel>,
    has_more: bool,
    last_id: Option<String>,
}

fn parse_anthropic_page(body: &[u8]) -> Result<AnthropicPage, InspectionCategory> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| InspectionCategory::MalformedResponse)?;
    let object = value
        .as_object()
        .ok_or(InspectionCategory::MalformedResponse)?;
    let entries = object
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or(InspectionCategory::MalformedResponse)?;
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry
            .as_object()
            .ok_or(InspectionCategory::MalformedResponse)?;
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or(InspectionCategory::MalformedResponse)?;
        let display_name = match entry.get("display_name") {
            Some(serde_json::Value::String(value)) => Some(value.clone()),
            None | Some(serde_json::Value::Null) => None,
            _ => return Err(InspectionCategory::MalformedResponse),
        };
        models.push(DiscoveredModel {
            id: id.to_owned(),
            display_name,
        });
    }
    Ok(AnthropicPage {
        models,
        has_more: object
            .get("has_more")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        last_id: object
            .get("last_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

async fn bounded_body(
    response: reqwest::Response,
    status: StatusCode,
    attempts: u32,
    started: Instant,
    endpoint_origin: &str,
) -> Result<Vec<u8>, InspectionFailure> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            inspection_failure(
                classify_reqwest_error(&error),
                Some(status.as_u16()),
                attempts,
                started,
                Some(endpoint_origin.to_owned()),
            )
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_DISCOVERY_BODY_BYTES {
            return Err(inspection_failure(
                InspectionCategory::ResponseTooLarge,
                Some(status.as_u16()),
                attempts,
                started,
                Some(endpoint_origin.to_owned()),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn status_category(status: StatusCode) -> InspectionCategory {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            InspectionCategory::AuthenticationRejected
        }
        StatusCode::TOO_MANY_REQUESTS => InspectionCategory::RateLimited,
        _ => InspectionCategory::UpstreamStatus,
    }
}

fn inspection_failure(
    category: InspectionCategory,
    http_status: Option<u16>,
    attempts: u32,
    started: Instant,
    endpoint_origin: Option<String>,
) -> InspectionFailure {
    InspectionFailure {
        category,
        http_status,
        attempts,
        elapsed_ms: elapsed_ms(started),
        endpoint_origin,
    }
}

fn unreachable(
    category: InspectionCategory,
    attempts: u32,
    started: Instant,
    endpoint_origin: Option<String>,
    retry_count: u8,
) -> ReachabilityResult {
    ReachabilityResult::Unreachable {
        failure: inspection_failure(category, None, attempts, started, endpoint_origin),
        checked_at_unix_ms: now_unix_ms(),
        retry_count,
    }
}

fn classify_reqwest_error(error: &reqwest::Error) -> InspectionCategory {
    if error.is_timeout() {
        return InspectionCategory::Timeout;
    }
    let mut diagnostics = String::new();
    let mut source = error.source();
    while let Some(error) = source {
        diagnostics.push(' ');
        diagnostics.push_str(&error.to_string());
        source = error.source();
    }
    let diagnostics = diagnostics.to_ascii_lowercase();
    if diagnostics.contains("dns")
        || diagnostics.contains("name resolution")
        || diagnostics.contains("failed to lookup")
    {
        InspectionCategory::Dns
    } else if diagnostics.contains("tls")
        || diagnostics.contains("certificate")
        || diagnostics.contains("rustls")
    {
        InspectionCategory::Tls
    } else {
        InspectionCategory::Connect
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    duration_ms(started.elapsed())
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_ms)
        .unwrap_or_default()
}
