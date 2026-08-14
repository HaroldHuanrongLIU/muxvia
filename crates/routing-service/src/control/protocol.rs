use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error, ser::SerializeStruct};
use serde_json::Value;
use uuid::Uuid;

use crate::service::provider_inspector::{ModelDiscoveryResult, ReachabilityResult};

pub const FRAME_LIMIT: u32 = 1_048_576;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RpcVersion;

impl RpcVersion {
    pub const V1_0: Self = Self;
}

impl Serialize for RpcVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut value = serializer.serialize_struct("RpcVersion", 2)?;
        value.serialize_field("major", &1)?;
        value.serialize_field("minor", &0)?;
        value.end()
    }
}

impl<'de> Deserialize<'de> for RpcVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireVersion {
            major: u8,
            minor: u8,
        }

        let value = WireVersion::deserialize(deserializer)?;
        if value.major == 1 && value.minor == 0 {
            Ok(Self::V1_0)
        } else {
            Err(D::Error::custom("unsupported-rpc-version"))
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameLimit;

impl FrameLimit {
    pub const V1: Self = Self;
}

impl Serialize for FrameLimit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(FRAME_LIMIT)
    }
}

impl<'de> Deserialize<'de> for FrameLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match u32::deserialize(deserializer)? {
            FRAME_LIMIT => Ok(Self::V1),
            _ => Err(D::Error::custom("invalid-frame-limit")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Codex,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ClientFrame {
    Hello {
        rpc: RpcVersion,
        release: String,
    },
    Request {
        request_id: String,
        operation: ControlOperation,
    },
    Cancel {
        request_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ServerFrame {
    HelloAck {
        rpc: RpcVersion,
        release: String,
        service_epoch: String,
        frame_limit: FrameLimit,
    },
    Response {
        request_id: String,
        result: ControlResult,
    },
    Error {
        request_id: Option<String>,
        problem: ControlProblem,
        #[serde(skip_serializing_if = "Option::is_none")]
        authoritative_view: Option<TargetView>,
    },
    TargetView {
        view: TargetView,
    },
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ControlOperation {
    OpenTarget {
        target: Target,
    },
    Act {
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        action: Value,
    },
    DiscoverModels {
        target: Target,
        source: DiscoverySource,
    },
    CheckReachability {
        target: Target,
        provider_id: Uuid,
        #[serde(deserialize_with = "deserialize_positive_provider_revision")]
        provider_revision: u64,
    },
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum DiscoverySource {
    Saved {
        provider_id: Uuid,
        #[serde(deserialize_with = "deserialize_positive_provider_revision")]
        provider_revision: u64,
    },
    Draft {
        base_url: String,
        credential_source: DraftCredentialSource,
    },
}

impl fmt::Debug for DiscoverySource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Saved {
                provider_id,
                provider_revision,
            } => formatter
                .debug_struct("Saved")
                .field("provider_id", provider_id)
                .field("provider_revision", provider_revision)
                .finish(),
            Self::Draft {
                base_url: _,
                credential_source,
            } => formatter
                .debug_struct("Draft")
                .field("base_url", &Redacted)
                .field("credential_source", credential_source)
                .finish(),
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum DraftCredentialSource {
    Missing,
    Ephemeral {
        value: String,
    },
    Saved {
        provider_id: Uuid,
        #[serde(deserialize_with = "deserialize_positive_provider_revision")]
        provider_revision: u64,
    },
}

impl fmt::Debug for DraftCredentialSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => formatter.write_str("Missing"),
            Self::Ephemeral { .. } => formatter
                .debug_struct("Ephemeral")
                .field("value", &Redacted)
                .finish(),
            Self::Saved {
                provider_id,
                provider_revision,
            } => formatter
                .debug_struct("Saved")
                .field("provider_id", provider_id)
                .field("provider_revision", provider_revision)
                .finish(),
        }
    }
}

impl fmt::Debug for ControlOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenTarget { target } => formatter
                .debug_struct("OpenTarget")
                .field("target", target)
                .finish(),
            Self::Act {
                target,
                action_id,
                expected_revision,
                ..
            } => formatter
                .debug_struct("Act")
                .field("target", target)
                .field("action_id", action_id)
                .field("expected_revision", expected_revision)
                .field("action", &Redacted)
                .finish(),
            Self::DiscoverModels { target, source } => formatter
                .debug_struct("DiscoverModels")
                .field("target", target)
                .field("source", source)
                .finish(),
            Self::CheckReachability {
                target,
                provider_id,
                provider_revision,
            } => formatter
                .debug_struct("CheckReachability")
                .field("target", target)
                .field("provider_id", provider_id)
                .field("provider_revision", provider_revision)
                .finish(),
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum TargetAction {
    CreateProvider {
        name: String,
        base_url: String,
        model: String,
        credential: CredentialEdit,
        preset_key: Option<String>,
    },
    UpdateProvider {
        provider_id: String,
        #[serde(deserialize_with = "deserialize_positive_provider_revision")]
        provider_revision: u64,
        name: String,
        base_url: String,
        model: String,
        credential: CredentialEdit,
    },
    ReorderProviders {
        provider_ids: Vec<Uuid>,
    },
    DeleteProvider {
        provider_id: Uuid,
        #[serde(deserialize_with = "deserialize_positive_provider_revision")]
        provider_revision: u64,
    },
    DuplicateProvider {
        source_provider_id: Uuid,
        #[serde(deserialize_with = "deserialize_positive_provider_revision")]
        source_provider_revision: u64,
        name: String,
        base_url: String,
        model: String,
        credential: DuplicateCredential,
    },
    ActivateProvider {
        provider_id: String,
        mode: ActivationMode,
    },
}

fn deserialize_positive_provider_revision<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let provider_revision = u64::deserialize(deserializer)?;
    if provider_revision == 0 {
        return Err(D::Error::custom("invalid-provider-revision"));
    }
    Ok(provider_revision)
}

impl fmt::Debug for TargetAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateProvider {
                name,
                base_url,
                model,
                credential,
                preset_key,
            } => formatter
                .debug_struct("CreateProvider")
                .field("name", name)
                .field("base_url", base_url)
                .field("model", model)
                .field("credential", credential)
                .field("preset_key", preset_key)
                .finish(),
            Self::UpdateProvider {
                provider_id,
                provider_revision,
                name,
                base_url,
                model,
                credential,
            } => formatter
                .debug_struct("UpdateProvider")
                .field("provider_id", provider_id)
                .field("provider_revision", provider_revision)
                .field("name", name)
                .field("base_url", base_url)
                .field("model", model)
                .field("credential", credential)
                .finish(),
            Self::ReorderProviders { provider_ids } => formatter
                .debug_struct("ReorderProviders")
                .field("provider_ids", provider_ids)
                .finish(),
            Self::DeleteProvider {
                provider_id,
                provider_revision,
            } => formatter
                .debug_struct("DeleteProvider")
                .field("provider_id", provider_id)
                .field("provider_revision", provider_revision)
                .finish(),
            Self::DuplicateProvider {
                source_provider_id,
                source_provider_revision,
                name,
                base_url,
                model,
                credential,
            } => formatter
                .debug_struct("DuplicateProvider")
                .field("source_provider_id", source_provider_id)
                .field("source_provider_revision", source_provider_revision)
                .field("name", name)
                .field("base_url", base_url)
                .field("model", model)
                .field("credential", credential)
                .finish(),
            Self::ActivateProvider { provider_id, mode } => formatter
                .debug_struct("ActivateProvider")
                .field("provider_id", provider_id)
                .field("mode", mode)
                .finish(),
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum CredentialEdit {
    Keep,
    Remove,
    Replace { value: String },
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum DuplicateCredential {
    Without,
    ReuseSource,
    Replace { value: String },
}

impl fmt::Debug for DuplicateCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Without => formatter.write_str("Without"),
            Self::ReuseSource => formatter.write_str("ReuseSource"),
            Self::Replace { .. } => formatter
                .debug_struct("Replace")
                .field("value", &Redacted)
                .finish(),
        }
    }
}

impl fmt::Debug for CredentialEdit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => formatter.write_str("Keep"),
            Self::Remove => formatter.write_str("Remove"),
            Self::Replace { .. } => formatter
                .debug_struct("Replace")
                .field("value", &Redacted)
                .finish(),
        }
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivationMode {
    Direct,
    Takeover,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ControlResult {
    TargetView { view: TargetView },
    ActionOutcome { outcome: ActionOutcome },
    ModelDiscovery { result: ModelDiscoveryResult },
    Reachability { result: ReachabilityResult },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ActionOutcome {
    pub status: ActionStatus,
    pub view: TargetView,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionStatus {
    Applied,
    Replayed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetView {
    pub target: Target,
    pub management_revision: u64,
    pub view_sequence: u64,
    pub service: ServiceView,
    pub mode: String,
    pub takeover: TakeoverView,
    pub providers: Vec<ProviderView>,
    pub provider_presets: Vec<ProviderPresetView>,
    pub current_provider_id: Option<String>,
    pub serving_provider_id: Option<String>,
    pub managed_configuration: ManagedConfigurationView,
    pub recovery: RecoveryView,
    pub activated_snapshot: Option<ActivatedSnapshotView>,
    pub problems: Vec<ControlProblem>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceView {
    pub epoch: String,
    pub state: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeoverView {
    pub state: String,
    pub endpoint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    pub id: Uuid,
    pub position: u32,
    pub provider_revision: u64,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub routing_requirement: ProviderRoutingRequirement,
    pub credential: CredentialPresence,
    pub completeness: ProviderCompleteness,
    pub missing_fields: Vec<ProviderRequirement>,
    pub provenance: Option<ProviderProvenanceView>,
    pub generated: bool,
    pub active_references: Vec<ProviderReferenceView>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProtocol {
    OpenaiResponses,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRoutingRequirement {
    DirectCompatible,
    TakeoverRequired,
}

impl fmt::Display for ProviderRoutingRequirement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DirectCompatible => "direct-compatible",
            Self::TakeoverRequired => "takeover-required",
        })
    }
}

impl fmt::Display for ProviderProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::OpenaiResponses => "openai-responses",
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderCompleteness {
    Complete,
    Incomplete,
}

impl fmt::Display for ProviderCompleteness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderRequirement {
    BaseUrl,
    Model,
    Credential,
}

impl fmt::Display for ProviderRequirement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BaseUrl => "base-url",
            Self::Model => "model",
            Self::Credential => "credential",
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProvenanceView {
    pub kind: String,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderReferenceView {
    Current,
    ActivatedSnapshot,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPresetView {
    pub key: String,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialPresence {
    Present,
    Missing,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedConfigurationView {
    pub state: String,
    pub path: Option<String>,
    pub restart_required: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryView {
    pub intent_id: Option<String>,
    pub state: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivatedSnapshotView {
    pub id: Uuid,
    pub provider_id: Uuid,
    pub model: String,
    pub epoch: Uuid,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ControlProblem {
    pub code: String,
    pub message: String,
}
