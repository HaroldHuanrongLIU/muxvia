use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use url::Url;
use uuid::Uuid;

use super::{
    accounts::{AccountAuthorizationState, SubscriptionAccountRecord, SubscriptionAccountStore},
    coordinator::SubscriptionAccountCoordinator,
    resolver::{
        ResolvedSubscriptionAccess, SubscriptionAccountResolution, SubscriptionAccountResolver,
    },
};
use crate::control::protocol::SubscriptionProviderBinding;

const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_AUTHORIZATION_ORIGIN: &str = "https://auth.openai.com";
const DEVICE_AUTHORIZATION_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_AUTHORIZATION_USER_AGENT: &str = "cc-switch-codex-oauth";
const DEVICE_AUTHORIZATION_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEFAULT_EXPIRES_IN_SECONDS: u64 = 900;

#[derive(Clone, Deserialize)]
pub(crate) struct RemoteDeviceChallenge {
    pub(crate) device_auth_id: String,
    pub(crate) user_code: String,
    pub(crate) interval: Option<Value>,
    pub(crate) expires_in: Option<u64>,
}

impl fmt::Debug for RemoteDeviceChallenge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteDeviceChallenge")
            .field("device_auth_id", &"<redacted>")
            .field("user_code_present", &!self.user_code.is_empty())
            .field("interval_present", &self.interval.is_some())
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DeviceAuthorizationError {
    #[error("device-authorization-failed")]
    Failed,
    #[error("device-authorization-flow-not-found")]
    FlowNotFound,
    #[error("device-authorization-identity-mismatch")]
    IdentityMismatch,
    #[error("subscription-account-needs-reauthorization")]
    NeedsReauthorization,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RemoteRefreshError {
    #[error("subscription-refresh-failed")]
    Failed,
    #[error("subscription-refresh-permanently-rejected")]
    PermanentRejection,
}

pub(crate) enum RemoteDevicePoll {
    Pending,
    Expired,
    Authorized {
        authorization_code: String,
        code_verifier: String,
    },
}

impl fmt::Debug for RemoteDevicePoll {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => formatter.write_str("Pending"),
            Self::Expired => formatter.write_str("Expired"),
            Self::Authorized { .. } => formatter.write_str("Authorized(<redacted>)"),
        }
    }
}

pub(crate) struct RemoteOAuthTokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) id_token: Option<String>,
    pub(crate) expires_in: Option<i64>,
}

impl fmt::Debug for RemoteOAuthTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteOAuthTokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token_present", &self.refresh_token.is_some())
            .field("id_token_present", &self.id_token.is_some())
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[async_trait]
pub(crate) trait DeviceAuthorizationAuthority: Send + Sync {
    async fn start(&self) -> Result<RemoteDeviceChallenge, DeviceAuthorizationError>;
    async fn poll(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> Result<RemoteDevicePoll, DeviceAuthorizationError>;
    async fn exchange(
        &self,
        authorization_code: &str,
        code_verifier: &str,
    ) -> Result<RemoteOAuthTokens, DeviceAuthorizationError>;
    async fn refresh(&self, refresh_token: &str) -> Result<RemoteOAuthTokens, RemoteRefreshError>;
}

pub(crate) struct ReqwestDeviceAuthorizationAuthority {
    client: reqwest::Client,
    start_url: Url,
    poll_url: Url,
    token_url: Url,
}

impl ReqwestDeviceAuthorizationAuthority {
    pub(crate) fn new() -> Result<Self, DeviceAuthorizationError> {
        Self::for_origin(DEVICE_AUTHORIZATION_ORIGIN)
    }

    #[cfg(test)]
    fn for_test_origin(origin: &str) -> Result<Self, DeviceAuthorizationError> {
        Self::for_origin(origin)
    }

    pub(crate) fn for_origin(origin: &str) -> Result<Self, DeviceAuthorizationError> {
        let origin = Url::parse(origin).map_err(|_| DeviceAuthorizationError::Failed)?;
        let client = reqwest::Client::builder()
            .user_agent(DEVICE_AUTHORIZATION_USER_AGENT)
            .build()
            .map_err(|_| DeviceAuthorizationError::Failed)?;
        Ok(Self {
            client,
            start_url: origin
                .join("/api/accounts/deviceauth/usercode")
                .map_err(|_| DeviceAuthorizationError::Failed)?,
            poll_url: origin
                .join("/api/accounts/deviceauth/token")
                .map_err(|_| DeviceAuthorizationError::Failed)?,
            token_url: origin
                .join("/oauth/token")
                .map_err(|_| DeviceAuthorizationError::Failed)?,
        })
    }

    async fn json_response<T: for<'de> Deserialize<'de>>(
        response: reqwest::Response,
    ) -> Result<T, DeviceAuthorizationError> {
        if !response.status().is_success() {
            return Err(DeviceAuthorizationError::Failed);
        }
        let body = response
            .bytes()
            .await
            .map_err(|_| DeviceAuthorizationError::Failed)?;
        serde_json::from_slice(&body).map_err(|_| DeviceAuthorizationError::Failed)
    }
}

#[derive(Deserialize)]
struct RemoteDeviceAuthorizedResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct RemoteOAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[async_trait]
impl DeviceAuthorizationAuthority for ReqwestDeviceAuthorizationAuthority {
    async fn start(&self) -> Result<RemoteDeviceChallenge, DeviceAuthorizationError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "client_id": DEVICE_AUTHORIZATION_CLIENT_ID,
        }))
        .map_err(|_| DeviceAuthorizationError::Failed)?;
        let response = self
            .client
            .post(self.start_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| DeviceAuthorizationError::Failed)?;
        Self::json_response(response).await
    }

    async fn poll(
        &self,
        device_auth_id: &str,
        user_code: &str,
    ) -> Result<RemoteDevicePoll, DeviceAuthorizationError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        }))
        .map_err(|_| DeviceAuthorizationError::Failed)?;
        let response = self
            .client
            .post(self.poll_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| DeviceAuthorizationError::Failed)?;
        match response.status() {
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND => {
                Ok(RemoteDevicePoll::Pending)
            }
            reqwest::StatusCode::GONE => Ok(RemoteDevicePoll::Expired),
            status if status.is_success() => {
                let authorized: RemoteDeviceAuthorizedResponse =
                    Self::json_response(response).await?;
                if authorized.authorization_code.is_empty() || authorized.code_verifier.is_empty() {
                    return Err(DeviceAuthorizationError::Failed);
                }
                Ok(RemoteDevicePoll::Authorized {
                    authorization_code: authorized.authorization_code,
                    code_verifier: authorized.code_verifier,
                })
            }
            _ => Err(DeviceAuthorizationError::Failed),
        }
    }

    async fn exchange(
        &self,
        authorization_code: &str,
        code_verifier: &str,
    ) -> Result<RemoteOAuthTokens, DeviceAuthorizationError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", authorization_code)
            .append_pair("redirect_uri", DEVICE_AUTHORIZATION_REDIRECT_URI)
            .append_pair("client_id", DEVICE_AUTHORIZATION_CLIENT_ID)
            .append_pair("code_verifier", code_verifier)
            .finish();
        let response = self
            .client
            .post(self.token_url.clone())
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| DeviceAuthorizationError::Failed)?;
        let tokens: RemoteOAuthTokenResponse = Self::json_response(response).await?;
        if tokens.access_token.is_empty() {
            return Err(DeviceAuthorizationError::Failed);
        }
        Ok(RemoteOAuthTokens {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            id_token: tokens.id_token,
            expires_in: tokens.expires_in,
        })
    }

    async fn refresh(&self, refresh_token: &str) -> Result<RemoteOAuthTokens, RemoteRefreshError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh_token)
            .append_pair("client_id", DEVICE_AUTHORIZATION_CLIENT_ID)
            .finish();
        let response = self
            .client
            .post(self.token_url.clone())
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| RemoteRefreshError::Failed)?;
        if matches!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(RemoteRefreshError::PermanentRejection);
        }
        if !response.status().is_success() {
            return Err(RemoteRefreshError::Failed);
        }
        let body = response
            .bytes()
            .await
            .map_err(|_| RemoteRefreshError::Failed)?;
        let tokens: RemoteOAuthTokenResponse =
            serde_json::from_slice(&body).map_err(|_| RemoteRefreshError::Failed)?;
        if tokens.access_token.is_empty() {
            return Err(RemoteRefreshError::Failed);
        }
        Ok(RemoteOAuthTokens {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            id_token: tokens.id_token,
            expires_in: tokens.expires_in,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceAuthorizationChallenge {
    pub(crate) flow_id: Uuid,
    pub(crate) user_code: String,
    pub(crate) verification_url: &'static str,
    pub(crate) expires_in_seconds: u64,
    pub(crate) poll_interval_seconds: u64,
}

#[derive(Clone)]
struct PendingFlow {
    remote_device_id: String,
    user_code: String,
    expires_at_seconds: u64,
    reauthorizing_account_id: Option<String>,
}

impl fmt::Debug for PendingFlow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingFlow")
            .field("remote_device_id", &"<redacted>")
            .field("user_code", &"<redacted>")
            .field("expires_at_seconds", &self.expires_at_seconds)
            .field("reauthorizing_account_id", &self.reauthorizing_account_id)
            .finish()
    }
}

pub(crate) struct DeviceAuthorizationManager {
    accounts: Arc<SubscriptionAccountStore>,
    coordinator: Arc<SubscriptionAccountCoordinator>,
    authority: Arc<dyn DeviceAuthorizationAuthority>,
    pending: Mutex<HashMap<Uuid, PendingFlow>>,
    access_tokens: RwLock<HashMap<String, CachedAccessToken>>,
}

struct CachedAccessToken {
    token: String,
    expires_at_seconds: u64,
}

impl fmt::Debug for CachedAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CachedAccessToken")
            .field("token", &"<redacted>")
            .field("expires_at_seconds", &self.expires_at_seconds)
            .finish()
    }
}

impl DeviceAuthorizationManager {
    pub(crate) fn new(
        accounts: Arc<SubscriptionAccountStore>,
        coordinator: Arc<SubscriptionAccountCoordinator>,
        authority: Arc<dyn DeviceAuthorizationAuthority>,
    ) -> Self {
        Self {
            accounts,
            coordinator,
            authority,
            pending: Mutex::new(HashMap::new()),
            access_tokens: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) async fn start(
        &self,
        reauthorizing_account_id: Option<String>,
    ) -> Result<DeviceAuthorizationChallenge, DeviceAuthorizationError> {
        let remote = self.authority.start().await?;
        if remote.device_auth_id.is_empty() || remote.user_code.is_empty() {
            return Err(DeviceAuthorizationError::Failed);
        }
        let expires_in_seconds = remote.expires_in.unwrap_or(DEFAULT_EXPIRES_IN_SECONDS);
        let flow_id = Uuid::new_v4();
        let expires_at_seconds = now_seconds().saturating_add(expires_in_seconds);
        self.pending
            .lock()
            .await
            .retain(|_, flow| flow.expires_at_seconds > now_seconds());
        self.pending.lock().await.insert(
            flow_id,
            PendingFlow {
                remote_device_id: remote.device_auth_id,
                user_code: remote.user_code.clone(),
                expires_at_seconds,
                reauthorizing_account_id,
            },
        );
        Ok(DeviceAuthorizationChallenge {
            flow_id,
            user_code: remote.user_code,
            verification_url: DEVICE_VERIFICATION_URL,
            expires_in_seconds,
            poll_interval_seconds: effective_poll_interval(remote.interval.as_ref()),
        })
    }

    pub(crate) async fn poll(
        &self,
        flow_id: Uuid,
    ) -> Result<DeviceAuthorizationPoll, DeviceAuthorizationError> {
        let flow = self
            .pending
            .lock()
            .await
            .get(&flow_id)
            .cloned()
            .ok_or(DeviceAuthorizationError::FlowNotFound)?;
        if flow.expires_at_seconds <= now_seconds() {
            self.pending.lock().await.remove(&flow_id);
            return Ok(DeviceAuthorizationPoll::Expired);
        }
        match self
            .authority
            .poll(&flow.remote_device_id, &flow.user_code)
            .await?
        {
            RemoteDevicePoll::Pending => Ok(DeviceAuthorizationPoll::Pending),
            RemoteDevicePoll::Expired => {
                self.pending.lock().await.remove(&flow_id);
                Ok(DeviceAuthorizationPoll::Expired)
            }
            RemoteDevicePoll::Authorized {
                authorization_code,
                code_verifier,
            } => {
                let tokens = self
                    .authority
                    .exchange(&authorization_code, &code_verifier)
                    .await?;
                let refresh_token = tokens
                    .refresh_token
                    .clone()
                    .filter(|value| !value.is_empty())
                    .ok_or(DeviceAuthorizationError::Failed)?;
                let (account_id, email) =
                    extract_identity(&tokens).ok_or(DeviceAuthorizationError::Failed)?;
                if flow
                    .reauthorizing_account_id
                    .as_ref()
                    .is_some_and(|expected| expected != &account_id)
                {
                    return Err(DeviceAuthorizationError::IdentityMismatch);
                }
                let expires_in = tokens.expires_in.unwrap_or(3600).max(0) as u64;
                Ok(DeviceAuthorizationPoll::Authorized {
                    authorization: AuthorizedSubscriptionAccount {
                        account: SubscriptionAccountRecord {
                            account_id: account_id.clone(),
                            email,
                            refresh_token,
                            authenticated_at: now_seconds() as i64,
                            state: AccountAuthorizationState::Authorized,
                        },
                        token: tokens.access_token,
                        expires_at_seconds: now_seconds().saturating_add(expires_in),
                    },
                })
            }
        }
    }

    pub(crate) async fn complete_authorization(
        &self,
        flow_id: Uuid,
        authorization: AuthorizedSubscriptionAccount,
    ) -> Result<String, DeviceAuthorizationError> {
        let account_id = authorization.account.account_id.clone();
        self.access_tokens.write().await.insert(
            account_id.clone(),
            CachedAccessToken {
                token: authorization.token,
                expires_at_seconds: authorization.expires_at_seconds,
            },
        );
        self.pending.lock().await.remove(&flow_id);
        Ok(account_id)
    }

    pub(crate) async fn reset_for_recovery_restore(&self) {
        self.pending.lock().await.clear();
        self.access_tokens.write().await.clear();
    }

    pub(crate) async fn access_token_for_account(
        &self,
        account_id: &str,
    ) -> Result<String, DeviceAuthorizationError> {
        let now = now_seconds();
        let snapshot = self
            .accounts
            .read()
            .map_err(|_| DeviceAuthorizationError::Failed)?;
        let Some(account) = snapshot.document.accounts.get(account_id).cloned() else {
            self.access_tokens.write().await.remove(account_id);
            return Err(DeviceAuthorizationError::Failed);
        };
        if account.state == AccountAuthorizationState::NeedsReauthorization {
            self.access_tokens.write().await.remove(account_id);
            return Err(DeviceAuthorizationError::NeedsReauthorization);
        }
        if let Some(cached) = self.access_tokens.read().await.get(account_id)
            && cached.expires_at_seconds > now.saturating_add(60)
        {
            return Ok(cached.token.clone());
        }
        let tokens = match self.authority.refresh(&account.refresh_token).await {
            Ok(tokens) => tokens,
            Err(RemoteRefreshError::Failed) => return Err(DeviceAuthorizationError::Failed),
            Err(RemoteRefreshError::PermanentRejection) => {
                let publication = self
                    .coordinator
                    .record_refresh(
                        Uuid::new_v4(),
                        &account,
                        None,
                        None,
                        AccountAuthorizationState::NeedsReauthorization,
                    )
                    .await
                    .map_err(|_| DeviceAuthorizationError::Failed)?;
                self.coordinator.publish(publication).await;
                self.access_tokens.write().await.remove(account_id);
                return Err(DeviceAuthorizationError::NeedsReauthorization);
            }
        };
        if tokens.access_token.is_empty() {
            return Err(DeviceAuthorizationError::Failed);
        }
        if extract_identity(&tokens).is_some_and(|(identity, _)| identity != account.account_id) {
            return Err(DeviceAuthorizationError::IdentityMismatch);
        }

        let rotated_refresh_token = tokens
            .refresh_token
            .as_ref()
            .filter(|value| !value.is_empty())
            .map(String::as_str);
        let publication = self
            .coordinator
            .record_refresh(
                Uuid::new_v4(),
                &account,
                rotated_refresh_token,
                Some(now as i64),
                AccountAuthorizationState::Authorized,
            )
            .await
            .map_err(|_| DeviceAuthorizationError::Failed)?;
        self.coordinator.publish(publication).await;

        let expires_in = tokens.expires_in.unwrap_or(3600).max(0) as u64;
        self.access_tokens.write().await.insert(
            account_id.to_owned(),
            CachedAccessToken {
                token: tokens.access_token.clone(),
                expires_at_seconds: now.saturating_add(expires_in),
            },
        );
        Ok(tokens.access_token)
    }
}

#[async_trait]
impl SubscriptionAccountResolver for DeviceAuthorizationManager {
    async fn resolve_subscription_account(
        &self,
        binding: &SubscriptionProviderBinding,
    ) -> Result<ResolvedSubscriptionAccess, SubscriptionAccountResolution> {
        let account_id = match binding {
            SubscriptionProviderBinding::Fixed { account_id } => account_id.clone(),
            SubscriptionProviderBinding::FollowDefault => self
                .accounts
                .read()
                .ok()
                .and_then(|snapshot| snapshot.document.default_account_id)
                .ok_or(SubscriptionAccountResolution::Unavailable)?,
        };
        let access_token = self
            .access_token_for_account(&account_id)
            .await
            .map_err(|error| match error {
                DeviceAuthorizationError::NeedsReauthorization => {
                    SubscriptionAccountResolution::NeedsReauthorization
                }
                DeviceAuthorizationError::Failed
                | DeviceAuthorizationError::FlowNotFound
                | DeviceAuthorizationError::IdentityMismatch => {
                    SubscriptionAccountResolution::Unavailable
                }
            })?;
        Ok(ResolvedSubscriptionAccess::new(
            account_id,
            SecretString::from(access_token),
        ))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AuthorizedSubscriptionAccount {
    pub(crate) account: SubscriptionAccountRecord,
    token: String,
    expires_at_seconds: u64,
}

impl fmt::Debug for AuthorizedSubscriptionAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizedSubscriptionAccount")
            .field("account", &self.account)
            .field("token", &"<redacted>")
            .field("expires_at_seconds", &self.expires_at_seconds)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeviceAuthorizationPoll {
    Pending,
    Expired,
    Authorized {
        authorization: AuthorizedSubscriptionAccount,
    },
}

#[derive(Default, Deserialize)]
struct IdentityClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    organizations: Vec<OrganizationClaim>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiAuthClaim>,
}

#[derive(Default, Deserialize)]
struct OrganizationClaim {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Default, Deserialize)]
struct OpenAiAuthClaim {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

fn extract_identity(tokens: &RemoteOAuthTokens) -> Option<(String, Option<String>)> {
    let id_claims = tokens.id_token.as_deref().and_then(parse_claims);
    let access_claims = parse_claims(&tokens.access_token);
    let account_id = id_claims
        .as_ref()
        .and_then(account_identity)
        .or_else(|| access_claims.as_ref().and_then(account_identity))?;
    let email = id_claims
        .as_ref()
        .and_then(|claims| claims.email.clone())
        .or_else(|| access_claims.and_then(|claims| claims.email));
    Some((account_id, email))
}

fn account_identity(claims: &IdentityClaims) -> Option<String> {
    claims
        .chatgpt_account_id
        .clone()
        .or_else(|| {
            claims
                .openai_auth
                .as_ref()
                .and_then(|auth| auth.chatgpt_account_id.clone())
        })
        .or_else(|| claims.organizations.first().and_then(|org| org.id.clone()))
}

fn parse_claims(token: &str) -> Option<IdentityClaims> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn effective_poll_interval(value: Option<&Value>) -> u64 {
    let raw = match value {
        Some(Value::Number(number)) => number.as_u64().unwrap_or(5),
        Some(Value::String(value)) => value.parse().unwrap_or(5),
        _ => 5,
    };
    let backend = raw.max(1).saturating_add(3);
    backend.saturating_add(3).max(8)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        net::Ipv4Addr,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use base64::Engine as _;
    use secrecy::ExposeSecret;
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use uuid::Uuid;

    use super::{
        DeviceAuthorizationAuthority, DeviceAuthorizationError, DeviceAuthorizationManager,
        DeviceAuthorizationPoll, RemoteDeviceChallenge, RemoteDevicePoll, RemoteOAuthTokens,
        RemoteRefreshError, ReqwestDeviceAuthorizationAuthority,
    };
    use crate::{
        control::protocol::SubscriptionProviderBinding,
        home::MuxviaHome,
        state::StateStore,
        subscription::accounts::{
            AccountAuthorizationState, SubscriptionAccountDocument, SubscriptionAccountRecord,
            SubscriptionAccountStore,
        },
        subscription::coordinator::SubscriptionAccountCoordinator,
        subscription::resolver::{SubscriptionAccountResolution, SubscriptionAccountResolver},
    };

    struct StartAuthority {
        starts: Mutex<u32>,
    }

    #[async_trait]
    impl DeviceAuthorizationAuthority for StartAuthority {
        async fn start(&self) -> Result<RemoteDeviceChallenge, DeviceAuthorizationError> {
            *self.starts.lock().expect("start count") += 1;
            Ok(RemoteDeviceChallenge {
                device_auth_id: "REMOTE_DEVICE_ID_SECRET_11741".to_owned(),
                user_code: "ABCD-EFGH".to_owned(),
                interval: Some(serde_json::json!("REMOTE_INTERVAL_SECRET_11742")),
                expires_in: Some(900),
            })
        }

        async fn poll(
            &self,
            _device_auth_id: &str,
            _user_code: &str,
        ) -> Result<RemoteDevicePoll, DeviceAuthorizationError> {
            Err(DeviceAuthorizationError::Failed)
        }

        async fn exchange(
            &self,
            _authorization_code: &str,
            _code_verifier: &str,
        ) -> Result<RemoteOAuthTokens, DeviceAuthorizationError> {
            Err(DeviceAuthorizationError::Failed)
        }

        async fn refresh(
            &self,
            _refresh_token: &str,
        ) -> Result<RemoteOAuthTokens, RemoteRefreshError> {
            Err(RemoteRefreshError::Failed)
        }
    }

    struct ScriptedAuthority {
        polls: Mutex<VecDeque<RemoteDevicePoll>>,
        exchanged_verifier: Mutex<Option<String>>,
    }

    struct RefreshAuthority {
        result: Mutex<Option<Result<RemoteOAuthTokens, RemoteRefreshError>>>,
        refresh_calls: Mutex<u32>,
    }

    struct ReauthorizationAuthority {
        account_id: &'static str,
    }

    #[test]
    fn remote_challenge_debug_redacts_raw_polling_metadata() {
        let challenge = RemoteDeviceChallenge {
            device_auth_id: "REMOTE_DEVICE_DEBUG_SECRET_11743".to_owned(),
            user_code: "DEBUG-CODE".to_owned(),
            interval: Some(serde_json::json!("REMOTE_INTERVAL_DEBUG_SECRET_11744")),
            expires_in: Some(900),
        };
        let diagnostic = format!("{challenge:?}");
        for secret in [
            "REMOTE_DEVICE_DEBUG_SECRET_11743",
            "REMOTE_INTERVAL_DEBUG_SECRET_11744",
        ] {
            assert!(
                !diagnostic.contains(secret),
                "remote challenge diagnostic exposed upstream authorization material"
            );
        }
    }

    #[async_trait]
    impl DeviceAuthorizationAuthority for ReauthorizationAuthority {
        async fn start(&self) -> Result<RemoteDeviceChallenge, DeviceAuthorizationError> {
            Ok(RemoteDeviceChallenge {
                device_auth_id: "REAUTH_DEVICE_SECRET_11821".to_owned(),
                user_code: "REAUTH-CODE".to_owned(),
                interval: Some(serde_json::json!(5)),
                expires_in: Some(900),
            })
        }

        async fn poll(
            &self,
            _device_auth_id: &str,
            _user_code: &str,
        ) -> Result<RemoteDevicePoll, DeviceAuthorizationError> {
            Ok(RemoteDevicePoll::Authorized {
                authorization_code: "REAUTH_AUTHORIZATION_SECRET_11822".to_owned(),
                code_verifier: "REAUTH_VERIFIER_SECRET_11823".to_owned(),
            })
        }

        async fn exchange(
            &self,
            _authorization_code: &str,
            _code_verifier: &str,
        ) -> Result<RemoteOAuthTokens, DeviceAuthorizationError> {
            let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&serde_json::json!({
                    "chatgpt_account_id": self.account_id,
                    "email": "reauthorized@example.test"
                }))
                .expect("reauthorization claims"),
            );
            Ok(RemoteOAuthTokens {
                access_token: "REAUTH_ACCESS_SECRET_11824".to_owned(),
                refresh_token: Some("REAUTH_REFRESH_SECRET_11825".to_owned()),
                id_token: Some(format!("e30.{claims}.signature")),
                expires_in: Some(3600),
            })
        }

        async fn refresh(
            &self,
            _refresh_token: &str,
        ) -> Result<RemoteOAuthTokens, RemoteRefreshError> {
            Err(RemoteRefreshError::Failed)
        }
    }

    #[async_trait]
    impl DeviceAuthorizationAuthority for RefreshAuthority {
        async fn start(&self) -> Result<RemoteDeviceChallenge, DeviceAuthorizationError> {
            Err(DeviceAuthorizationError::Failed)
        }

        async fn poll(
            &self,
            _device_auth_id: &str,
            _user_code: &str,
        ) -> Result<RemoteDevicePoll, DeviceAuthorizationError> {
            Err(DeviceAuthorizationError::Failed)
        }

        async fn exchange(
            &self,
            _authorization_code: &str,
            _code_verifier: &str,
        ) -> Result<RemoteOAuthTokens, DeviceAuthorizationError> {
            Err(DeviceAuthorizationError::Failed)
        }

        async fn refresh(
            &self,
            refresh_token: &str,
        ) -> Result<RemoteOAuthTokens, RemoteRefreshError> {
            assert!(
                refresh_token == "REFRESH_TOKEN_OLD_SECRET_11771",
                "refresh used the wrong stored token"
            );
            *self.refresh_calls.lock().expect("refresh calls") += 1;
            self.result
                .lock()
                .expect("refresh result")
                .take()
                .expect("unexpected repeated refresh")
        }
    }

    #[async_trait]
    impl DeviceAuthorizationAuthority for ScriptedAuthority {
        async fn start(&self) -> Result<RemoteDeviceChallenge, DeviceAuthorizationError> {
            Ok(RemoteDeviceChallenge {
                device_auth_id: "REMOTE_DEVICE_ID_SECRET_11751".to_owned(),
                user_code: "IJKL-MNOP".to_owned(),
                interval: Some(serde_json::json!(5)),
                expires_in: Some(900),
            })
        }

        async fn poll(
            &self,
            device_auth_id: &str,
            user_code: &str,
        ) -> Result<RemoteDevicePoll, DeviceAuthorizationError> {
            assert!(
                device_auth_id == "REMOTE_DEVICE_ID_SECRET_11751",
                "remote device id changed"
            );
            assert!(user_code == "IJKL-MNOP", "poll user code changed");
            self.polls
                .lock()
                .expect("poll queue")
                .pop_front()
                .ok_or(DeviceAuthorizationError::Failed)
        }

        async fn exchange(
            &self,
            authorization_code: &str,
            code_verifier: &str,
        ) -> Result<RemoteOAuthTokens, DeviceAuthorizationError> {
            assert!(
                authorization_code == "AUTHORIZATION_CODE_SECRET_11752",
                "authorization code changed"
            );
            *self.exchanged_verifier.lock().expect("exchange verifier") =
                Some(code_verifier.to_owned());
            Ok(RemoteOAuthTokens {
                access_token: "ACCESS_TOKEN_SECRET_11753".to_owned(),
                refresh_token: Some("REFRESH_TOKEN_SECRET_11754".to_owned()),
                id_token: Some(concat!(
                    "e30.",
                    "eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NvdW50LXByaW1hcnkiLCJlbWFpbCI6Im9wZXJhdG9yQGV4YW1wbGUudGVzdCJ9",
                    ".signature"
                ).to_owned()),
                expires_in: Some(3600),
            })
        }

        async fn refresh(
            &self,
            _refresh_token: &str,
        ) -> Result<RemoteOAuthTokens, RemoteRefreshError> {
            Err(RemoteRefreshError::Failed)
        }
    }

    #[tokio::test]
    async fn start_exposes_only_the_fixed_public_challenge_and_effective_interval() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        let authority = Arc::new(StartAuthority {
            starts: Mutex::new(0),
        });
        let coordinator = account_coordinator(&home, accounts.clone()).await;
        let manager = DeviceAuthorizationManager::new(accounts, coordinator, authority.clone());

        let challenge = manager.start(None).await.expect("start authorization");
        assert!(
            challenge.flow_id.get_version_num() == 4,
            "flow identity was not random v4"
        );
        assert!(challenge.user_code == "ABCD-EFGH", "user code changed");
        assert!(
            challenge.verification_url == "https://auth.openai.com/codex/device",
            "verification URL changed"
        );
        assert!(challenge.expires_in_seconds == 900, "expiry changed");
        assert!(
            challenge.poll_interval_seconds == 11,
            "effective polling interval changed"
        );
        assert!(
            *authority.starts.lock().expect("start count") == 1,
            "start request repeated"
        );

        let diagnostic = format!("{challenge:?}");
        assert!(
            !diagnostic.contains("REMOTE_DEVICE_ID_SECRET_11741"),
            "public challenge exposed the remote device identity"
        );
        assert!(
            !diagnostic.contains("REMOTE_INTERVAL_SECRET_11742"),
            "public challenge exposed raw upstream polling metadata"
        );
    }

    #[tokio::test]
    async fn recovery_restore_clears_pending_flows_and_cached_access_tokens() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        let authority = Arc::new(StartAuthority {
            starts: Mutex::new(0),
        });
        let coordinator = account_coordinator(&home, accounts.clone()).await;
        let manager = DeviceAuthorizationManager::new(accounts, coordinator, authority);
        manager.start(None).await.expect("pending authorization");
        manager.access_tokens.write().await.insert(
            "account-before-restore".to_owned(),
            super::CachedAccessToken {
                token: "ACCESS_TOKEN_BEFORE_RESTORE_SECRET_18041".to_owned(),
                expires_at_seconds: u64::MAX,
            },
        );

        manager.reset_for_recovery_restore().await;

        assert!(manager.pending.lock().await.is_empty());
        assert!(manager.access_tokens.read().await.is_empty());
    }

    #[tokio::test]
    async fn pending_then_success_uses_server_verifier_and_persists_only_refresh_token() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        let authority = Arc::new(ScriptedAuthority {
            polls: Mutex::new(VecDeque::from([
                RemoteDevicePoll::Pending,
                RemoteDevicePoll::Authorized {
                    authorization_code: "AUTHORIZATION_CODE_SECRET_11752".to_owned(),
                    code_verifier: "SERVER_VERIFIER_SECRET_11755".to_owned(),
                },
            ])),
            exchanged_verifier: Mutex::new(None),
        });
        let coordinator = account_coordinator(&home, accounts.clone()).await;
        let manager = DeviceAuthorizationManager::new(
            accounts.clone(),
            coordinator.clone(),
            authority.clone(),
        );
        let challenge = manager.start(None).await.expect("start authorization");

        let pending = manager.poll(challenge.flow_id).await.expect("pending poll");
        assert!(
            pending == DeviceAuthorizationPoll::Pending,
            "pending poll changed state"
        );
        assert!(
            !home.subscription_accounts_path().exists(),
            "pending poll wrote account file"
        );

        let authorized = manager
            .poll(challenge.flow_id)
            .await
            .expect("authorized poll");
        let DeviceAuthorizationPoll::Authorized { authorization } = authorized else {
            panic!("authorized poll returned the wrong state");
        };
        assert!(
            authorization.account.account_id == "account-primary",
            "authorized poll returned the wrong account"
        );
        let state = Arc::new(StateStore::open(&home).await.expect("state store"));
        let coordinator = SubscriptionAccountCoordinator::new(state, accounts.clone());
        coordinator
            .record_authorization(challenge.flow_id, authorization.account.clone())
            .await
            .expect("commit authorized account");
        manager
            .complete_authorization(challenge.flow_id, authorization)
            .await
            .expect("complete authorized flow");
        assert!(
            authority
                .exchanged_verifier
                .lock()
                .expect("exchange verifier")
                .as_deref()
                == Some("SERVER_VERIFIER_SECRET_11755"),
            "token exchange did not use the server verifier"
        );
        let file = fs::read_to_string(home.subscription_accounts_path()).expect("account file");
        assert!(
            file.contains("REFRESH_TOKEN_SECRET_11754"),
            "refresh token was not persisted"
        );
        assert!(
            !file.contains("ACCESS_TOKEN_SECRET_11753"),
            "access token was persisted"
        );
        assert!(
            !file.contains("SERVER_VERIFIER_SECRET_11755"),
            "server verifier was persisted"
        );
        let snapshot = accounts.read().expect("reopen account document");
        assert!(
            snapshot.document.default_account_id.as_deref() == Some("account-primary")
                && snapshot.document.accounts.contains_key("account-primary"),
            "authorized account identity/default was not persisted"
        );
    }

    #[tokio::test]
    async fn production_http_adapter_uses_the_pinned_three_request_contract() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("device authority listener");
        let address = listener.local_addr().expect("device authority address");
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            for response in [
                r#"{"device_auth_id":"REMOTE_DEVICE_HTTP_SECRET_11761","user_code":"QRST-UVWX","interval":"5","expires_in":900}"#,
                r#"{"authorization_code":"AUTHORIZATION_HTTP_SECRET_11762","code_verifier":"VERIFIER_HTTP_SECRET_11763"}"#,
                concat!(
                    "{\"access_token\":\"ACCESS_HTTP_SECRET_11764\",",
                    "\"refresh_token\":\"REFRESH_HTTP_SECRET_11765\",",
                    "\"id_token\":\"e30.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NvdW50LXByaW1hcnkiLCJlbWFpbCI6Im9wZXJhdG9yQGV4YW1wbGUudGVzdCJ9.signature\",",
                    "\"expires_in\":3600}"
                ),
            ] {
                let (mut socket, _) = listener.accept().await.expect("authority accept");
                let request = read_http_request(&mut socket).await;
                captured.lock().expect("captured requests").push(request);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                socket
                    .write_all(reply.as_bytes())
                    .await
                    .expect("authority response");
            }
        });

        let authority =
            ReqwestDeviceAuthorizationAuthority::for_test_origin(&format!("http://{address}"))
                .expect("device authority adapter");
        let started = authority.start().await.expect("start request");
        let polled = authority
            .poll(&started.device_auth_id, &started.user_code)
            .await
            .expect("poll request");
        let RemoteDevicePoll::Authorized {
            authorization_code,
            code_verifier,
        } = polled
        else {
            panic!("poll did not authorize");
        };
        let tokens = authority
            .exchange(&authorization_code, &code_verifier)
            .await
            .expect("exchange request");
        assert!(
            tokens.refresh_token.is_some(),
            "exchange omitted refresh token"
        );
        server.await.expect("authority server");

        let requests = requests.lock().expect("captured requests");
        assert!(
            requests.len() == 3,
            "device authority request count changed"
        );
        assert!(
            requests[0].starts_with("POST /api/accounts/deviceauth/usercode HTTP/1.1")
                && requests[0]
                    .to_ascii_lowercase()
                    .contains("content-type: application/json")
                && requests[0]
                    .to_ascii_lowercase()
                    .contains("user-agent: cc-switch-codex-oauth")
                && requests[0].contains("\"client_id\":\"app_EMoamEEZ73f0CkXaXp7hrann\""),
            "device start request departed from the pinned contract"
        );
        assert!(
            requests[1].starts_with("POST /api/accounts/deviceauth/token HTTP/1.1")
                && requests[1].contains("\"device_auth_id\":\"REMOTE_DEVICE_HTTP_SECRET_11761\"")
                && requests[1].contains("\"user_code\":\"QRST-UVWX\""),
            "device poll request departed from the pinned contract"
        );
        assert!(
            requests[2].starts_with("POST /oauth/token HTTP/1.1")
                && requests[2]
                    .to_ascii_lowercase()
                    .contains("content-type: application/x-www-form-urlencoded")
                && requests[2].contains("grant_type=authorization_code")
                && requests[2].contains("code=AUTHORIZATION_HTTP_SECRET_11762")
                && requests[2].contains("code_verifier=VERIFIER_HTTP_SECRET_11763")
                && requests[2]
                    .contains("redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback")
                && requests[2].contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"),
            "token exchange request departed from the pinned contract"
        );
    }

    #[tokio::test]
    async fn refresh_rotates_private_refresh_token_and_keeps_access_token_in_memory_only() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        install_authorized_account(&accounts);
        let authority = Arc::new(RefreshAuthority {
            result: Mutex::new(Some(Ok(RemoteOAuthTokens {
                access_token: "ACCESS_TOKEN_ROTATED_SECRET_11772".to_owned(),
                refresh_token: Some("REFRESH_TOKEN_ROTATED_SECRET_11773".to_owned()),
                id_token: None,
                expires_in: Some(3600),
            }))),
            refresh_calls: Mutex::new(0),
        });
        let state = Arc::new(StateStore::open(&home).await.expect("state store"));
        let coordinator = Arc::new(SubscriptionAccountCoordinator::new(state, accounts.clone()));
        let mut publications = coordinator.subscribe();
        let manager = DeviceAuthorizationManager::new(
            accounts.clone(),
            coordinator.clone(),
            authority.clone(),
        );

        let access = manager
            .access_token_for_account("account-primary")
            .await
            .expect("refresh access token");
        assert!(
            access == "ACCESS_TOKEN_ROTATED_SECRET_11772",
            "refresh returned the wrong access token"
        );
        let again = manager
            .access_token_for_account("account-primary")
            .await
            .expect("reuse cached access token");
        assert!(again == access, "cached access token changed");
        assert!(
            *authority.refresh_calls.lock().expect("refresh calls") == 1,
            "cached access token caused another refresh"
        );
        let file = fs::read_to_string(home.subscription_accounts_path()).expect("account file");
        assert!(
            file.contains("REFRESH_TOKEN_ROTATED_SECRET_11773"),
            "rotated refresh token was not persisted"
        );
        assert!(
            !file.contains("ACCESS_TOKEN_ROTATED_SECRET_11772"),
            "access token was persisted"
        );
        let catalog = coordinator.catalog().await.expect("refreshed catalog");
        assert!(
            catalog.revision == 1 && catalog.view_sequence == 1,
            "refresh did not commit through the durable account coordinator"
        );
        let publication =
            tokio::time::timeout(std::time::Duration::from_secs(1), publications.recv())
                .await
                .expect("refresh did not publish the committed catalog")
                .expect("refresh publication channel closed");
        assert!(
            publication == catalog,
            "refresh published a non-authoritative catalog"
        );
    }

    #[tokio::test]
    async fn deleting_an_account_invalidates_its_cached_access_token() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        install_authorized_account(&accounts);
        let authority = Arc::new(RefreshAuthority {
            result: Mutex::new(Some(Ok(RemoteOAuthTokens {
                access_token: "DELETED_ACCOUNT_ACCESS_SECRET_12211".to_owned(),
                refresh_token: None,
                id_token: None,
                expires_in: Some(3600),
            }))),
            refresh_calls: Mutex::new(0),
        });
        let state = Arc::new(StateStore::open(&home).await.expect("state store"));
        let coordinator = Arc::new(SubscriptionAccountCoordinator::new(state, accounts.clone()));
        let manager = DeviceAuthorizationManager::new(accounts, coordinator.clone(), authority);

        manager
            .access_token_for_account("account-primary")
            .await
            .expect("prime access-token cache");
        coordinator
            .apply(
                Uuid::new_v4(),
                1,
                crate::control::protocol::SubscriptionAccountAction::DeleteAccount {
                    account_id: "account-primary".to_owned(),
                },
            )
            .await
            .expect("delete cached account");
        let result = manager.access_token_for_account("account-primary").await;
        assert!(
            matches!(result, Err(DeviceAuthorizationError::Failed)),
            "deleted account retained a usable cached access token"
        );
    }

    #[tokio::test]
    async fn permanent_refresh_rejection_persists_needs_reauthorization_without_retrying() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        install_authorized_account(&accounts);
        let authority = Arc::new(RefreshAuthority {
            result: Mutex::new(Some(Err(RemoteRefreshError::PermanentRejection))),
            refresh_calls: Mutex::new(0),
        });
        let coordinator = account_coordinator(&home, accounts.clone()).await;
        let manager = DeviceAuthorizationManager::new(
            accounts.clone(),
            coordinator.clone(),
            authority.clone(),
        );

        let error = manager
            .access_token_for_account("account-primary")
            .await
            .expect_err("permanent rejection returned an access token");
        assert!(
            error == DeviceAuthorizationError::NeedsReauthorization,
            "permanent rejection returned the wrong fixed error"
        );
        let snapshot = accounts.read().expect("account state after rejection");
        let account = snapshot
            .document
            .accounts
            .get("account-primary")
            .expect("authorized account disappeared");
        assert!(
            account.state == AccountAuthorizationState::NeedsReauthorization
                && snapshot.document.default_account_id.as_deref() == Some("account-primary"),
            "permanent rejection changed identity/default instead of requiring reauthorization"
        );
        let catalog = coordinator
            .catalog()
            .await
            .expect("reauthorization catalog");
        assert!(
            catalog.revision == 1
                && catalog.view_sequence == 1
                && catalog.accounts[0].state
                    == crate::control::protocol::SubscriptionAccountState::NeedsReauthorization,
            "permanent rejection did not commit through the durable account coordinator"
        );
        let repeated = manager
            .access_token_for_account("account-primary")
            .await
            .expect_err("reauthorization state retried refresh");
        assert!(
            repeated == DeviceAuthorizationError::NeedsReauthorization
                && *authority.refresh_calls.lock().expect("refresh calls") == 1,
            "reauthorization state retried the rejected refresh token"
        );
    }

    #[tokio::test]
    async fn subscription_binding_resolution_is_exact_redacted_and_request_scoped() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        install_authorized_account(&accounts);
        let authority = Arc::new(RefreshAuthority {
            result: Mutex::new(Some(Ok(RemoteOAuthTokens {
                access_token: "RESOLVED_ACCESS_TOKEN_SECRET_11901".to_owned(),
                refresh_token: None,
                id_token: None,
                expires_in: Some(3600),
            }))),
            refresh_calls: Mutex::new(0),
        });
        let manager = DeviceAuthorizationManager::new(
            accounts.clone(),
            account_coordinator(&home, accounts.clone()).await,
            authority,
        );

        let fixed = manager
            .resolve_subscription_account(&SubscriptionProviderBinding::Fixed {
                account_id: "account-primary".to_owned(),
            })
            .await
            .expect("fixed binding resolution");
        assert!(
            fixed.account_id() == "account-primary"
                && fixed.access_token().expose_secret() == "RESOLVED_ACCESS_TOKEN_SECRET_11901",
            "fixed binding resolved a different identity or token"
        );
        let diagnostic = format!("{fixed:?}");
        assert!(
            !diagnostic.contains("account-primary")
                && !diagnostic.contains("RESOLVED_ACCESS_TOKEN_SECRET_11901"),
            "resolved subscription access diagnostic exposed private material"
        );

        let missing = manager
            .resolve_subscription_account(&SubscriptionProviderBinding::Fixed {
                account_id: "missing-account".to_owned(),
            })
            .await;
        assert!(
            matches!(missing, Err(SubscriptionAccountResolution::Unavailable)),
            "dangling fixed binding did not fail without account substitution"
        );

        let snapshot = accounts.read().expect("account document");
        let mut no_default = snapshot.document.clone();
        no_default.default_account_id = None;
        accounts
            .replace(&snapshot, &no_default)
            .expect("remove default account");
        let follow = manager
            .resolve_subscription_account(&SubscriptionProviderBinding::FollowDefault)
            .await;
        assert!(
            matches!(follow, Err(SubscriptionAccountResolution::Unavailable)),
            "follow-default binding did not reread the missing default"
        );
    }

    #[tokio::test]
    async fn reauthorization_preserves_exact_identity_and_rejects_account_substitution() {
        for (remote_account, succeeds) in
            [("account-primary", true), ("account-substitution", false)]
        {
            let temp = TempDir::new().expect("temporary home");
            let home = MuxviaHome::from_user_home(temp.path());
            let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
            install_authorized_account(&accounts);
            let snapshot = accounts.read().expect("authorized account");
            let mut reauthorization = snapshot.document.clone();
            reauthorization
                .accounts
                .get_mut("account-primary")
                .expect("primary account")
                .state = AccountAuthorizationState::NeedsReauthorization;
            accounts
                .replace(&snapshot, &reauthorization)
                .expect("mark reauthorization state");
            let manager = DeviceAuthorizationManager::new(
                accounts.clone(),
                account_coordinator(&home, accounts.clone()).await,
                Arc::new(ReauthorizationAuthority {
                    account_id: remote_account,
                }),
            );
            let challenge = manager
                .start(Some("account-primary".to_owned()))
                .await
                .expect("start reauthorization");
            let result = manager.poll(challenge.flow_id).await;
            let diagnostic = format!("{result:?}");
            if succeeds {
                let authorization = match result {
                    Ok(DeviceAuthorizationPoll::Authorized { authorization }) => authorization,
                    _ => panic!("same-identity reauthorization did not authorize"),
                };
                assert!(
                    authorization.account.account_id == "account-primary",
                    "same-identity reauthorization changed identity"
                );
                let state = Arc::new(StateStore::open(&home).await.expect("state store"));
                let coordinator = SubscriptionAccountCoordinator::new(state, accounts.clone());
                coordinator
                    .record_authorization(challenge.flow_id, authorization.account.clone())
                    .await
                    .expect("commit reauthorization");
                manager
                    .complete_authorization(challenge.flow_id, authorization)
                    .await
                    .expect("complete reauthorization");
                let after = accounts.read().expect("account after reauthorization");
                assert!(
                    after.document.accounts.len() == 1
                        && after.document.default_account_id.as_deref() == Some("account-primary")
                        && after.document.accounts["account-primary"].state
                            == AccountAuthorizationState::Authorized
                        && after.document.accounts["account-primary"].refresh_token
                            == "REAUTH_REFRESH_SECRET_11825",
                    "same-identity reauthorization did not preserve the account identity/default"
                );
            } else {
                let after = accounts.read().expect("account after reauthorization");
                assert!(
                    result == Err(DeviceAuthorizationError::IdentityMismatch)
                        && after.document == reauthorization,
                    "reauthorization substituted a different account identity"
                );
            }
            for secret in [
                "REAUTH_AUTHORIZATION_SECRET_11822",
                "REAUTH_VERIFIER_SECRET_11823",
                "REAUTH_ACCESS_SECRET_11824",
                "REAUTH_REFRESH_SECRET_11825",
            ] {
                assert!(
                    !diagnostic.contains(secret),
                    "reauthorization diagnostic exposed private OAuth material"
                );
            }
        }
    }

    fn install_authorized_account(accounts: &SubscriptionAccountStore) {
        let snapshot = accounts.read().expect("empty account document");
        let desired = SubscriptionAccountDocument {
            version: 1,
            accounts: std::collections::BTreeMap::from([(
                "account-primary".to_owned(),
                SubscriptionAccountRecord {
                    account_id: "account-primary".to_owned(),
                    email: Some("operator@example.test".to_owned()),
                    refresh_token: "REFRESH_TOKEN_OLD_SECRET_11771".to_owned(),
                    authenticated_at: 1_700_000_000,
                    state: AccountAuthorizationState::Authorized,
                },
            )]),
            default_account_id: Some("account-primary".to_owned()),
        };
        accounts
            .replace(&snapshot, &desired)
            .expect("install authorized account");
    }

    async fn account_coordinator(
        home: &MuxviaHome,
        accounts: Arc<SubscriptionAccountStore>,
    ) -> Arc<SubscriptionAccountCoordinator> {
        Arc::new(SubscriptionAccountCoordinator::new(
            Arc::new(StateStore::open(home).await.expect("state store")),
            accounts,
        ))
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = socket
                .read(&mut buffer)
                .await
                .expect("authority request read");
            assert!(read != 0, "authority request ended before headers");
            bytes.extend_from_slice(&buffer[..read]);
            let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + content_length {
                return String::from_utf8(bytes).expect("authority request utf8");
            }
        }
    }
}
