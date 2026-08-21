use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use secrecy::SecretString;
use tokio::sync::Mutex;
use url::Url;
use uuid::Uuid;

use crate::{
    claude::ClaudeConfigCodec,
    codex::CodexConfigCodec,
    control::protocol::{
        CredentialPresence, ExportedFailoverDraft, ExportedTargetProvider,
        ExportedUniversalProvider, ProviderAuthentication, ProviderConfigurationExport,
        ProviderConfigurationFormat, ProviderConfigurationVersion, ProviderImportCandidateView,
        ProviderImportPreview, ProviderImportProduct, ProviderImportSource,
        ProviderImportSourceTarget, ProviderImportSourceView, ProviderProtocol,
        ProviderRoutingRequirement, Target, TargetView, UniversalProviderTargetDraft,
    },
    domain::provider::normalize_provider_base_url,
    home::MuxviaHome,
    state::StateStore,
};

const MAX_SOURCE_BYTES: usize = 524_288;
const MAX_NAME_BYTES: usize = 256;
const MAX_MODEL_BYTES: usize = 256;
const MAX_CREDENTIAL_BYTES: usize = 16_384;
const MAX_PENDING_PREVIEWS: usize = 32;
const MAX_PROVIDER_COUNT: usize = 256;
const PREVIEW_LIFETIME: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum ProviderTransferError {
    #[error("provider import is invalid")]
    InvalidImport,
    #[error("provider import is too large")]
    ImportTooLarge,
    #[error("provider import is hostile")]
    HostileImport,
    #[error("state store operation failed")]
    State,
}

pub struct ProviderTransferService {
    store: Arc<StateStore>,
    home: MuxviaHome,
    pending: Mutex<PendingPreviews>,
}

struct PendingPreviews {
    entries: HashMap<Uuid, PendingPreview>,
    order: VecDeque<Uuid>,
}

struct PendingPreview {
    created_at: Instant,
    target: Target,
    candidates: Vec<PendingCandidate>,
}

enum PendingCandidate {
    Target(PendingTargetProvider),
    Universal(PendingUniversalProvider),
}

struct PendingTargetProvider {
    candidate_id: Uuid,
    target: Target,
    name: String,
    base_url: String,
    model: String,
    protocol: ProviderProtocol,
    authentication: ProviderAuthentication,
    routing_requirement: ProviderRoutingRequirement,
    credential: Option<SecretString>,
    imported_current: bool,
}

struct PendingUniversalProvider {
    candidate_id: Uuid,
    name: String,
    base_url: String,
    targets: Vec<UniversalProviderTargetDraft>,
    credential: Option<SecretString>,
}

impl ProviderTransferService {
    pub fn new(store: Arc<StateStore>, home: MuxviaHome) -> Self {
        Self {
            store,
            home,
            pending: Mutex::new(PendingPreviews {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    pub async fn preview(
        &self,
        target: Target,
        source: ProviderImportSource,
    ) -> Result<ProviderImportPreview, ProviderTransferError> {
        let _supported_home = self.home.user_home();
        let (source, candidates) = match source {
            ProviderImportSource::CcSwitch { payload } => {
                let candidate = parse_ccswitch_provider(target, &payload)?;
                (
                    ProviderImportSourceView {
                        product: ProviderImportProduct::CcSwitch,
                        target: source_target(target),
                    },
                    vec![PendingCandidate::Target(candidate)],
                )
            }
            ProviderImportSource::LiveTarget => {
                let candidate = match target {
                    Target::Codex => {
                        let (_source_identifier, name, model, base_url, credential) =
                            CodexConfigCodec::for_user_home(self.home.user_home())
                                .and_then(|codec| codec.provider_for_import())
                                .map_err(|_| ProviderTransferError::InvalidImport)?;
                        PendingTargetProvider {
                            candidate_id: Uuid::new_v4(),
                            target,
                            name,
                            base_url: normalize_provider_base_url(&base_url)
                                .map_err(|_| ProviderTransferError::HostileImport)?,
                            model,
                            protocol: ProviderProtocol::OpenaiResponses,
                            authentication: ProviderAuthentication::OpenaiBearer,
                            routing_requirement: ProviderRoutingRequirement::DirectCompatible,
                            credential: Some(credential),
                            imported_current: true,
                        }
                    }
                    Target::Claude => {
                        let (model, base_url, authentication, credential) =
                            ClaudeConfigCodec::for_user_home(self.home.user_home())
                                .and_then(|codec| codec.provider_for_import())
                                .map_err(|_| ProviderTransferError::InvalidImport)?;
                        PendingTargetProvider {
                            candidate_id: Uuid::new_v4(),
                            target,
                            name: "Imported Claude configuration".to_owned(),
                            base_url: normalize_provider_base_url(&base_url)
                                .map_err(|_| ProviderTransferError::HostileImport)?,
                            model,
                            protocol: ProviderProtocol::AnthropicMessages,
                            authentication,
                            routing_requirement: ProviderRoutingRequirement::DirectCompatible,
                            credential: Some(credential),
                            imported_current: true,
                        }
                    }
                };
                (
                    ProviderImportSourceView {
                        product: ProviderImportProduct::TargetCli,
                        target: source_target(target),
                    },
                    vec![PendingCandidate::Target(candidate)],
                )
            }
            ProviderImportSource::MuxviaExport { payload } => {
                let candidates = parse_muxvia_export(&payload)?;
                (
                    ProviderImportSourceView {
                        product: ProviderImportProduct::Muxvia,
                        target: ProviderImportSourceTarget::Universal,
                    },
                    candidates,
                )
            }
        };

        let mut projected = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            match candidate {
                PendingCandidate::Target(candidate) => {
                    let exact_matches = self
                        .store
                        .exact_target_provider_import_matches(
                            candidate.target,
                            candidate.base_url.clone(),
                            candidate.model.clone(),
                            candidate.protocol,
                            candidate.authentication,
                            candidate.routing_requirement.clone(),
                            candidate.credential.clone(),
                        )
                        .await
                        .map_err(|_| ProviderTransferError::State)?;
                    projected.push(ProviderImportCandidateView::TargetProvider {
                        candidate_id: candidate.candidate_id,
                        target: candidate.target,
                        name: candidate.name.clone(),
                        base_url: candidate.base_url.clone(),
                        model: candidate.model.clone(),
                        protocol: candidate.protocol,
                        authentication: candidate.authentication,
                        routing_requirement: candidate.routing_requirement.clone(),
                        credential: if candidate.credential.is_some() {
                            CredentialPresence::Present
                        } else {
                            CredentialPresence::Missing
                        },
                        imported_current: candidate.imported_current,
                        exact_matches,
                    });
                }
                PendingCandidate::Universal(candidate) => {
                    let exact_matches = self
                        .store
                        .exact_universal_provider_import_matches(
                            candidate.base_url.clone(),
                            candidate.targets.clone(),
                            candidate.credential.clone(),
                        )
                        .await
                        .map_err(|_| ProviderTransferError::State)?;
                    projected.push(ProviderImportCandidateView::UniversalProvider {
                        candidate_id: candidate.candidate_id,
                        name: candidate.name.clone(),
                        base_url: candidate.base_url.clone(),
                        credential: if candidate.credential.is_some() {
                            CredentialPresence::Present
                        } else {
                            CredentialPresence::Missing
                        },
                        targets: candidate.targets.clone(),
                        exact_matches,
                    });
                }
            }
        }

        let preview_token = Uuid::new_v4();
        let mut pending = self.pending.lock().await;
        pending.prune();
        pending.insert(
            preview_token,
            PendingPreview {
                created_at: Instant::now(),
                target,
                candidates,
            },
        );
        Ok(ProviderImportPreview {
            preview_token,
            source,
            candidates: projected,
        })
    }

    pub async fn export(&self) -> Result<ProviderConfigurationExport, ProviderTransferError> {
        let (catalog, codex, claude) = self
            .store
            .provider_configuration_export_views()
            .await
            .map_err(|_| ProviderTransferError::State)?;
        let universal_providers = catalog
            .providers
            .into_iter()
            .map(|provider| ExportedUniversalProvider {
                source_id: provider.id,
                position: provider.position,
                name: provider.name,
                base_url: provider.base_url,
                targets: provider
                    .targets
                    .into_iter()
                    .map(|overlay| UniversalProviderTargetDraft {
                        target: overlay.target,
                        enabled: overlay.enabled,
                        model: overlay.model,
                        authentication: overlay.authentication,
                        routing_requirement: overlay.routing_requirement,
                    })
                    .collect(),
            })
            .collect();
        let target_providers = [&codex, &claude]
            .into_iter()
            .flat_map(|view| {
                view.providers
                    .iter()
                    .map(|provider| ExportedTargetProvider {
                        source_id: provider.id,
                        target: view.target,
                        position: provider.position,
                        name: provider.name.clone(),
                        base_url: provider.base_url.clone(),
                        model: provider.model.clone(),
                        protocol: provider.protocol,
                        authentication: provider.authentication,
                        routing_requirement: provider.routing_requirement.clone(),
                        universal_provider_source_id: provider.universal_provider_id,
                    })
            })
            .collect();
        let failover_drafts = [&codex, &claude]
            .into_iter()
            .map(export_failover_draft)
            .collect();
        Ok(ProviderConfigurationExport {
            format: ProviderConfigurationFormat,
            version: ProviderConfigurationVersion,
            universal_providers,
            target_providers,
            failover_drafts,
        })
    }
}

fn export_failover_draft(view: &TargetView) -> ExportedFailoverDraft {
    ExportedFailoverDraft {
        target: view.target,
        provider_source_ids: view
            .failover
            .draft_members
            .iter()
            .map(|member| member.provider_id)
            .collect(),
    }
}

fn parse_muxvia_export(payload: &str) -> Result<Vec<PendingCandidate>, ProviderTransferError> {
    if payload.len() > MAX_SOURCE_BYTES {
        return Err(ProviderTransferError::ImportTooLarge);
    }
    let export: ProviderConfigurationExport =
        serde_json::from_str(payload).map_err(|_| ProviderTransferError::InvalidImport)?;
    if export.universal_providers.len() + export.target_providers.len() > MAX_PROVIDER_COUNT {
        return Err(ProviderTransferError::ImportTooLarge);
    }

    let mut source_ids = HashSet::new();
    for source_id in export
        .universal_providers
        .iter()
        .map(|provider| provider.source_id)
        .chain(
            export
                .target_providers
                .iter()
                .map(|provider| provider.source_id),
        )
    {
        if !source_ids.insert(source_id) {
            return Err(ProviderTransferError::HostileImport);
        }
    }

    validate_export_positions(&export)?;
    validate_export_failover(&export)?;

    let mut generated_source_ids = HashSet::new();
    for universal in &export.universal_providers {
        validate_universal_declaration(universal)?;
        for overlay in &universal.targets {
            let generated = export
                .target_providers
                .iter()
                .filter(|provider| {
                    provider.universal_provider_source_id == Some(universal.source_id)
                        && provider.target == overlay.target
                })
                .collect::<Vec<_>>();
            if overlay.enabled {
                if generated.len() != 1
                    || !generated_matches_universal(universal, overlay, generated[0])
                {
                    return Err(ProviderTransferError::HostileImport);
                }
                generated_source_ids.insert(generated[0].source_id);
            } else if !generated.is_empty() {
                return Err(ProviderTransferError::HostileImport);
            }
        }
    }
    if export.target_providers.iter().any(|provider| {
        provider.universal_provider_source_id.is_some()
            && !generated_source_ids.contains(&provider.source_id)
    }) {
        return Err(ProviderTransferError::HostileImport);
    }

    let mut candidates = Vec::new();
    let mut normalized = HashSet::new();
    let mut universal = export.universal_providers;
    universal.sort_by_key(|provider| provider.position);
    for provider in universal {
        let base_url = normalize_export_url(&provider.base_url)?;
        let key = universal_declaration_key(&base_url, &provider.targets);
        if !normalized.insert(key) {
            return Err(ProviderTransferError::HostileImport);
        }
        candidates.push(PendingCandidate::Universal(PendingUniversalProvider {
            candidate_id: Uuid::new_v4(),
            name: provider.name.trim().to_owned(),
            base_url,
            targets: provider.targets,
            credential: None,
        }));
    }

    let mut target_providers = export
        .target_providers
        .into_iter()
        .filter(|provider| provider.universal_provider_source_id.is_none())
        .collect::<Vec<_>>();
    target_providers.sort_by_key(|provider| (target_order(provider.target), provider.position));
    for provider in target_providers {
        validate_target_declaration(&provider)?;
        let base_url = normalize_export_url(&provider.base_url)?;
        let key = target_declaration_key(&provider, &base_url);
        if !normalized.insert(key) {
            return Err(ProviderTransferError::HostileImport);
        }
        candidates.push(PendingCandidate::Target(PendingTargetProvider {
            candidate_id: Uuid::new_v4(),
            target: provider.target,
            name: provider.name.trim().to_owned(),
            base_url,
            model: provider.model.trim().to_owned(),
            protocol: provider.protocol,
            authentication: provider.authentication,
            routing_requirement: provider.routing_requirement,
            credential: None,
            imported_current: false,
        }));
    }
    Ok(candidates)
}

fn validate_export_positions(
    export: &ProviderConfigurationExport,
) -> Result<(), ProviderTransferError> {
    validate_positions(
        export
            .universal_providers
            .iter()
            .map(|provider| provider.position),
    )?;
    for target in [Target::Codex, Target::Claude] {
        validate_positions(
            export
                .target_providers
                .iter()
                .filter(|provider| provider.target == target)
                .map(|provider| provider.position),
        )?;
    }
    Ok(())
}

fn validate_positions(positions: impl Iterator<Item = u32>) -> Result<(), ProviderTransferError> {
    let mut positions = positions.collect::<Vec<_>>();
    positions.sort_unstable();
    if positions
        .iter()
        .enumerate()
        .any(|(expected, position)| *position as usize != expected)
    {
        return Err(ProviderTransferError::HostileImport);
    }
    Ok(())
}

fn validate_export_failover(
    export: &ProviderConfigurationExport,
) -> Result<(), ProviderTransferError> {
    let known = export
        .target_providers
        .iter()
        .map(|provider| (provider.source_id, provider.target))
        .collect::<HashMap<_, _>>();
    let mut targets = HashSet::new();
    for draft in &export.failover_drafts {
        if !targets.insert(draft.target) {
            return Err(ProviderTransferError::HostileImport);
        }
        let mut members = HashSet::new();
        for source_id in &draft.provider_source_ids {
            if !members.insert(*source_id) || known.get(source_id) != Some(&draft.target) {
                return Err(ProviderTransferError::HostileImport);
            }
        }
    }
    if targets != HashSet::from([Target::Codex, Target::Claude]) {
        return Err(ProviderTransferError::HostileImport);
    }
    Ok(())
}

fn validate_universal_declaration(
    provider: &ExportedUniversalProvider,
) -> Result<(), ProviderTransferError> {
    bounded_text(&provider.name, MAX_NAME_BYTES, false)?;
    normalize_export_url(&provider.base_url)?;
    if provider.targets.len() != 2 {
        return Err(ProviderTransferError::HostileImport);
    }
    let mut targets = HashSet::new();
    for overlay in &provider.targets {
        if !targets.insert(overlay.target) {
            return Err(ProviderTransferError::HostileImport);
        }
        bounded_text(&overlay.model, MAX_MODEL_BYTES, !overlay.enabled)?;
        validate_target_authentication(overlay.target, overlay.authentication)?;
    }
    if targets != HashSet::from([Target::Codex, Target::Claude]) {
        return Err(ProviderTransferError::HostileImport);
    }
    Ok(())
}

fn validate_target_declaration(
    provider: &ExportedTargetProvider,
) -> Result<(), ProviderTransferError> {
    bounded_text(&provider.name, MAX_NAME_BYTES, false)?;
    bounded_text(&provider.model, MAX_MODEL_BYTES, false)?;
    normalize_export_url(&provider.base_url)?;
    validate_target_protocol(provider.target, provider.protocol)?;
    validate_target_authentication(provider.target, provider.authentication)
}

fn validate_target_protocol(
    target: Target,
    protocol: ProviderProtocol,
) -> Result<(), ProviderTransferError> {
    if matches!(
        (target, protocol),
        (Target::Codex, ProviderProtocol::OpenaiResponses)
            | (Target::Claude, ProviderProtocol::AnthropicMessages)
    ) {
        Ok(())
    } else {
        Err(ProviderTransferError::HostileImport)
    }
}

fn validate_target_authentication(
    target: Target,
    authentication: ProviderAuthentication,
) -> Result<(), ProviderTransferError> {
    if matches!(
        (target, authentication),
        (
            Target::Codex,
            ProviderAuthentication::OpenaiBearer | ProviderAuthentication::CodexSubscription
        ) | (
            Target::Claude,
            ProviderAuthentication::AnthropicApiKey | ProviderAuthentication::AnthropicBearer
        )
    ) {
        Ok(())
    } else {
        Err(ProviderTransferError::HostileImport)
    }
}

fn generated_matches_universal(
    universal: &ExportedUniversalProvider,
    overlay: &UniversalProviderTargetDraft,
    generated: &ExportedTargetProvider,
) -> bool {
    let generated_base_url = normalize_export_url(&generated.base_url).ok();
    let universal_base_url = normalize_export_url(&universal.base_url).ok();
    generated.name == universal.name
        && generated_base_url.is_some()
        && generated_base_url == universal_base_url
        && generated.model == overlay.model
        && generated.authentication == overlay.authentication
        && generated.routing_requirement == overlay.routing_requirement
        && validate_target_protocol(generated.target, generated.protocol).is_ok()
}

fn normalize_export_url(value: &str) -> Result<String, ProviderTransferError> {
    if value.len() > MAX_SOURCE_BYTES {
        return Err(ProviderTransferError::ImportTooLarge);
    }
    normalize_provider_base_url(value).map_err(|_| ProviderTransferError::HostileImport)
}

fn bounded_text(
    value: &str,
    max_bytes: usize,
    optional: bool,
) -> Result<(), ProviderTransferError> {
    let trimmed = value.trim();
    if (!optional && trimmed.is_empty()) || trimmed.len() > max_bytes || trimmed != value {
        return Err(ProviderTransferError::HostileImport);
    }
    Ok(())
}

fn universal_declaration_key(base_url: &str, targets: &[UniversalProviderTargetDraft]) -> String {
    let mut targets = targets.to_vec();
    targets.sort_by_key(|overlay| target_order(overlay.target));
    serde_json::to_string(&(base_url, targets)).expect("provider declaration serializes")
}

fn target_declaration_key(provider: &ExportedTargetProvider, base_url: &str) -> String {
    format!(
        "target:{:?}:{base_url}:{}:{:?}:{:?}:{:?}",
        provider.target,
        provider.model,
        provider.protocol,
        provider.authentication,
        provider.routing_requirement
    )
}

fn target_order(target: Target) -> u8 {
    match target {
        Target::Codex => 0,
        Target::Claude => 1,
    }
}

impl PendingPreviews {
    fn prune(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|_, preview| now.duration_since(preview.created_at) < PREVIEW_LIFETIME);
        self.order.retain(|token| self.entries.contains_key(token));
        while self.entries.len() >= MAX_PENDING_PREVIEWS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn insert(&mut self, token: Uuid, preview: PendingPreview) {
        self.entries.insert(token, preview);
        self.order.push_back(token);
    }
}

fn parse_ccswitch_provider(
    expected_target: Target,
    payload: &str,
) -> Result<PendingTargetProvider, ProviderTransferError> {
    if payload.len() > MAX_SOURCE_BYTES {
        return Err(ProviderTransferError::ImportTooLarge);
    }
    let url = Url::parse(payload).map_err(|_| ProviderTransferError::InvalidImport)?;
    if url.scheme() != "ccswitch" || url.host_str() != Some("v1") || url.path() != "/import" {
        return Err(ProviderTransferError::InvalidImport);
    }
    let allowed = [
        "resource",
        "app",
        "name",
        "homepage",
        "endpoint",
        "apiKey",
        "model",
        "notes",
        "haikuModel",
        "sonnetModel",
        "opusModel",
        "icon",
        "enabled",
    ];
    let allowed = allowed.into_iter().collect::<HashSet<_>>();
    let mut fields = HashMap::new();
    for (key, value) in url.query_pairs() {
        if !allowed.contains(key.as_ref())
            || fields
                .insert(key.into_owned(), value.into_owned())
                .is_some()
        {
            return Err(ProviderTransferError::HostileImport);
        }
    }
    if fields.get("resource").map(String::as_str) != Some("provider") {
        return Err(ProviderTransferError::InvalidImport);
    }
    let target = match fields.get("app").map(String::as_str) {
        Some("codex") => Target::Codex,
        Some("claude") => Target::Claude,
        _ => return Err(ProviderTransferError::InvalidImport),
    };
    if target != expected_target {
        return Err(ProviderTransferError::InvalidImport);
    }
    let name = bounded_trimmed(fields.get("name"), MAX_NAME_BYTES, false)?;
    let model = bounded_trimmed(fields.get("model"), MAX_MODEL_BYTES, true)?;
    let base_url = bounded_trimmed(fields.get("endpoint"), MAX_SOURCE_BYTES, true)?;
    let base_url = if base_url.is_empty() {
        base_url
    } else {
        normalize_provider_base_url(&base_url).map_err(|_| ProviderTransferError::HostileImport)?
    };
    let credential = match fields.get("apiKey") {
        None => None,
        Some(value) if value.trim().is_empty() || value.len() > MAX_CREDENTIAL_BYTES => {
            return Err(ProviderTransferError::InvalidImport);
        }
        Some(value) => Some(SecretString::from(value.clone())),
    };
    let (protocol, authentication) = match target {
        Target::Codex => (
            ProviderProtocol::OpenaiResponses,
            ProviderAuthentication::OpenaiBearer,
        ),
        Target::Claude => (
            ProviderProtocol::AnthropicMessages,
            ProviderAuthentication::AnthropicBearer,
        ),
    };
    Ok(PendingTargetProvider {
        candidate_id: Uuid::new_v4(),
        target,
        name,
        base_url,
        model,
        protocol,
        authentication,
        routing_requirement: ProviderRoutingRequirement::DirectCompatible,
        credential,
        imported_current: false,
    })
}

fn bounded_trimmed(
    value: Option<&String>,
    max_bytes: usize,
    optional: bool,
) -> Result<String, ProviderTransferError> {
    let value = value.map_or("", String::as_str).trim();
    if (!optional && value.is_empty()) || value.len() > max_bytes {
        return Err(ProviderTransferError::InvalidImport);
    }
    Ok(value.to_owned())
}

fn source_target(target: Target) -> ProviderImportSourceTarget {
    match target {
        Target::Codex => ProviderImportSourceTarget::Codex,
        Target::Claude => ProviderImportSourceTarget::Claude,
    }
}
