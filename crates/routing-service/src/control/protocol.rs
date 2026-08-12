use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error, ser::SerializeStruct};
use serde_json::Value;
use uuid::Uuid;

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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum TargetAction {
    SaveProvider {
        name: String,
        base_url: String,
        model: String,
        credential: String,
    },
    ActivateProvider {
        provider_id: String,
        mode: TakeoverMode,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TakeoverMode {
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
    pub current_provider_id: Option<String>,
    pub serving_provider_id: Option<String>,
    pub managed_configuration: ManagedConfigurationView,
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
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub credential: CredentialPresence,
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
