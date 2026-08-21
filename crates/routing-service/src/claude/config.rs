use std::{
    collections::BTreeSet,
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use secrecy::SecretString;
use serde::{Deserialize, Serialize, de::Error as _};
use serde_json::{Map, Value};

use super::ClaudeProblem;
use crate::{
    config::managed_file::{FileIdentity, ManagedFile, ManagedFileError, PreRenameHook},
    control::protocol::{
        ClaudeBlockingSelector, ClaudeHostManagedState, ClaudePreflightContext,
        ClaudeSelectorState, ProviderAuthentication, ShadowSource, Target,
    },
    state::{RecoveryIntent, RecoveryState, StateStore},
};

const BASE_URL_KEY: &str = "ANTHROPIC_BASE_URL";
const AUTH_TOKEN_KEY: &str = "ANTHROPIC_AUTH_TOKEN";
const MODEL_KEY: &str = "ANTHROPIC_MODEL";
const API_KEY: &str = "ANTHROPIC_API_KEY";

const PROVIDER_SELECTORS: [ClaudeBlockingSelector; 5] = [
    ClaudeBlockingSelector::Bedrock,
    ClaudeBlockingSelector::Vertex,
    ClaudeBlockingSelector::Foundry,
    ClaudeBlockingSelector::Mantle,
    ClaudeBlockingSelector::AnthropicAws,
];

type ClaudePreRenameHook = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaudeConfigOwnership {
    LegacyThree,
    FourField,
}

impl ClaudeConfigOwnership {
    pub(crate) fn from_managed_config_version(version: u32) -> Option<Self> {
        match version {
            1 => Some(Self::LegacyThree),
            2 => Some(Self::FourField),
            _ => None,
        }
    }

    fn owned_keys(self) -> &'static [&'static str] {
        match self {
            Self::LegacyThree => &[BASE_URL_KEY, AUTH_TOKEN_KEY, MODEL_KEY],
            Self::FourField => &[BASE_URL_KEY, AUTH_TOKEN_KEY, MODEL_KEY, API_KEY],
        }
    }
}

impl Serialize for ClaudeConfigOwnership {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(match self {
            Self::LegacyThree => 1,
            Self::FourField => 2,
        })
    }
}

impl<'de> Deserialize<'de> for ClaudeConfigOwnership {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u8::deserialize(deserializer)? {
            1 => Ok(Self::LegacyThree),
            2 => Ok(Self::FourField),
            _ => Err(D::Error::custom(
                "invalid Claude configuration ownership version",
            )),
        }
    }
}

fn legacy_ownership() -> ClaudeConfigOwnership {
    ClaudeConfigOwnership::LegacyThree
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ManagedClaudeMode {
    Direct,
    Takeover,
}

fn takeover_mode() -> ManagedClaudeMode {
    ManagedClaudeMode::Takeover
}

fn is_takeover_mode(mode: &ManagedClaudeMode) -> bool {
    *mode == ManagedClaudeMode::Takeover
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnedClaudeState {
    base_url: Option<Value>,
    auth_token: Option<Value>,
    model: Option<Value>,
    #[serde(default)]
    api_key: Option<Value>,
}

impl fmt::Debug for OwnedClaudeState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnedClaudeState")
            .field("base_url_present", &self.base_url.is_some())
            .field("auth_token_present", &self.auth_token.is_some())
            .field("model_present", &self.model.is_some())
            .field("api_key_present", &self.api_key.is_some())
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct DesiredClaudeState {
    #[serde(default = "legacy_ownership")]
    ownership_version: ClaudeConfigOwnership,
    #[serde(default = "takeover_mode", skip_serializing_if = "is_takeover_mode")]
    mode: ManagedClaudeMode,
    owned: OwnedClaudeState,
}

impl fmt::Debug for DesiredClaudeState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DesiredClaudeState(<redacted>)")
    }
}

impl DesiredClaudeState {
    pub(crate) fn ownership(&self) -> ClaudeConfigOwnership {
        self.ownership_version
    }

    pub(crate) fn reconciliation_provider(
        &self,
    ) -> Option<(String, String, ProviderAuthentication, SecretString)> {
        let model = self.owned.model.as_ref()?.as_str()?.to_owned();
        let base_url = self.owned.base_url.as_ref()?.as_str()?.to_owned();
        let (authentication, credential) = match (
            self.owned.auth_token.as_ref().and_then(Value::as_str),
            self.owned.api_key.as_ref().and_then(Value::as_str),
        ) {
            (Some(token), None) => (ProviderAuthentication::AnthropicBearer, token),
            (None, Some(key)) => (ProviderAuthentication::AnthropicApiKey, key),
            _ => return None,
        };
        Some((
            model,
            base_url,
            authentication,
            SecretString::from(credential.to_owned()),
        ))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ManagedClaudeState {
    Unmanaged { snapshot: ClaudeConfigSnapshot },
    Direct { snapshot: ClaudeConfigSnapshot },
    Takeover { snapshot: ClaudeConfigSnapshot },
}

#[derive(Clone, Serialize)]
pub struct ClaudeConfigSnapshot {
    #[serde(default = "legacy_ownership")]
    ownership_version: ClaudeConfigOwnership,
    identity: FileIdentity,
    owned: OwnedClaudeState,
    unrelated_fingerprint: String,
    #[serde(skip)]
    unrelated: Option<Value>,
}

impl PartialEq for ClaudeConfigSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.ownership_version == other.ownership_version
            && self.owned == other.owned
            && self.unrelated_fingerprint == other.unrelated_fingerprint
    }
}

impl<'de> Deserialize<'de> for ClaudeConfigSnapshot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireSnapshot {
            #[serde(default = "legacy_ownership")]
            ownership_version: ClaudeConfigOwnership,
            identity: FileIdentity,
            owned: OwnedClaudeState,
            #[serde(default)]
            unrelated_fingerprint: Option<String>,
            #[serde(default)]
            unrelated: Option<Value>,
        }

        let wire = WireSnapshot::deserialize(deserializer)?;
        let unrelated_fingerprint = match (&wire.unrelated_fingerprint, &wire.unrelated) {
            (Some(fingerprint), _) => fingerprint.clone(),
            (None, Some(unrelated)) => semantic_fingerprint(unrelated).map_err(D::Error::custom)?,
            (None, None) => return Err(D::Error::custom("missing unrelated semantic fingerprint")),
        };
        Ok(Self {
            ownership_version: wire.ownership_version,
            identity: wire.identity,
            owned: wire.owned,
            unrelated_fingerprint,
            unrelated: wire.unrelated,
        })
    }
}

impl fmt::Debug for ClaudeConfigSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeConfigSnapshot")
            .field("identity", &self.identity)
            .field("ownership_version", &self.ownership_version)
            .field("owned", &self.owned)
            .field("unrelated", &"<fingerprint>")
            .finish()
    }
}

impl ClaudeConfigSnapshot {
    pub(crate) fn identity(&self) -> &FileIdentity {
        &self.identity
    }

    pub(crate) fn ownership(&self) -> ClaudeConfigOwnership {
        self.ownership_version
    }

    #[allow(dead_code)]
    pub(crate) fn owned_fingerprint(&self) -> String {
        semantic_fingerprint(&self.owned)
            .expect("serializing captured Claude semantics cannot fail")
    }

    #[allow(dead_code)]
    pub(crate) fn unrelated_fingerprint(&self) -> &str {
        &self.unrelated_fingerprint
    }

    #[allow(dead_code)]
    pub(crate) fn as_desired_like(&self, committed: &DesiredClaudeState) -> DesiredClaudeState {
        DesiredClaudeState {
            ownership_version: self.ownership_version,
            mode: committed.mode,
            owned: self.owned.clone(),
        }
    }

    pub(crate) fn as_adopted_direct(&self) -> DesiredClaudeState {
        DesiredClaudeState {
            ownership_version: self.ownership_version,
            mode: ManagedClaudeMode::Direct,
            owned: self.owned.clone(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn provider_matches(&self, desired: &DesiredClaudeState) -> bool {
        self.owned.base_url == desired.owned.base_url && self.owned.model == desired.owned.model
    }

    #[allow(dead_code)]
    pub(crate) fn credential_matches(&self, desired: &DesiredClaudeState) -> bool {
        self.owned.auth_token == desired.owned.auth_token
            && self.owned.api_key == desired.owned.api_key
    }

    pub(crate) fn recovery_before_with_latest_unrelated(&self, recovery_before: &Self) -> Self {
        Self {
            ownership_version: recovery_before.ownership_version,
            identity: self.identity.clone(),
            owned: recovery_before.owned.clone(),
            unrelated_fingerprint: self.unrelated_fingerprint.clone(),
            unrelated: self.unrelated.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeRuntimeShadow {
    SettingsFlag,
    ModelFlag,
    InteractiveModel,
    ResumedSession,
    ExternalEnvironment,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudePreflightReport {
    pub restart_required: bool,
    pub unobservable_shadows: [ClaudeRuntimeShadow; 5],
}

pub struct ClaudeConfigCodec {
    file: ManagedFile,
    configured_home: PathBuf,
    managed_settings_paths: Vec<PathBuf>,
}

impl ClaudeConfigCodec {
    pub fn for_user_home(user_home: &Path) -> Result<Self, ClaudeProblem> {
        let managed_settings_paths = default_managed_settings_paths(user_home);
        Self::build(user_home, managed_settings_paths, None)
    }

    pub fn for_user_home_with_pre_rename_hook(
        user_home: &Path,
        hook: ClaudePreRenameHook,
    ) -> Result<Self, ClaudeProblem> {
        Self::build(user_home, Vec::new(), Some(hook))
    }

    pub fn for_user_home_with_managed_settings(
        user_home: &Path,
        managed_settings_paths: Vec<PathBuf>,
    ) -> Result<Self, ClaudeProblem> {
        Self::build(user_home, managed_settings_paths, None)
    }

    fn build(
        user_home: &Path,
        managed_settings_paths: Vec<PathBuf>,
        hook: Option<PreRenameHook>,
    ) -> Result<Self, ClaudeProblem> {
        if !user_home.is_absolute() || !user_home.is_dir() {
            return Err(ClaudeProblem::new(
                "unsupported-configuration-home",
                Some(user_home),
            ));
        }
        let configured_home = user_home.join(".claude");
        let file = match hook {
            Some(hook) => {
                ManagedFile::with_pre_rename_hook(user_home, ".claude", "settings.json", hook)
            }
            None => ManagedFile::in_configuration_home(user_home, ".claude", "settings.json"),
        }
        .map_err(|error| map_file_error(error, Some(&configured_home)))?;
        Ok(Self {
            file,
            configured_home,
            managed_settings_paths,
        })
    }

    pub fn settings_path(&self) -> &Path {
        self.file.path()
    }

    pub fn inspect(&self) -> Result<ClaudeConfigSnapshot, ClaudeProblem> {
        self.inspect_with_ownership(ClaudeConfigOwnership::FourField)
    }

    pub(crate) fn provider_for_import(
        &self,
    ) -> Result<(String, String, ProviderAuthentication, SecretString), ClaudeProblem> {
        self.inspect()?
            .as_adopted_direct()
            .reconciliation_provider()
            .ok_or_else(|| ClaudeProblem::new("invalid-configuration", Some(self.settings_path())))
    }

    #[allow(dead_code)]
    pub(crate) fn reconciliation_snapshot(
        &self,
        context: &ClaudePreflightContext,
        ownership: ClaudeConfigOwnership,
    ) -> Result<(ClaudeConfigSnapshot, Vec<ShadowSource>), ClaudeProblem> {
        if !context.has_valid_blocking_selector() {
            return Err(ClaudeProblem::new(
                "preflight-context-required",
                Some(self.settings_path()),
            ));
        }
        let cwd = PathBuf::from(&context.cwd);
        if !cwd.is_absolute() {
            return Err(ClaudeProblem::new("preflight-context-required", None));
        }
        let cwd = std::fs::canonicalize(cwd)
            .map_err(|_| ClaudeProblem::new("preflight-context-required", None))?;
        if !cwd.is_dir() {
            return Err(ClaudeProblem::new("preflight-context-required", None));
        }
        if let Some(observed) = &context.claude_config_dir {
            let observed = PathBuf::from(observed);
            let resolved = std::fs::canonicalize(&observed).unwrap_or(observed);
            let supported = self
                .settings_path()
                .parent()
                .is_some_and(|parent| resolved == parent || resolved == self.configured_home);
            if !supported {
                return Err(ClaudeProblem::new(
                    "unsupported-configuration-home",
                    Some(&resolved),
                ));
            }
        }

        let (snapshot, user_document) = self.read_snapshot(ownership)?;
        let mut shadows = Vec::new();
        if matches!(
            context.host_managed_state,
            ClaudeHostManagedState::Managed | ClaudeHostManagedState::Unknown
        ) {
            shadows.push(ShadowSource::ClaudeHostManaged);
        } else if matches!(
            context.selector_state,
            ClaudeSelectorState::Enabled | ClaudeSelectorState::UnknownNonempty
        ) {
            shadows.push(ShadowSource::ClaudeSelector(
                context
                    .blocking_selector
                    .expect("validated Claude context has an exact selector"),
            ));
        }
        collect_provider_mode_shadows(&user_document, &mut shadows);

        let mut shadow_paths = self.managed_settings_paths.clone();
        shadow_paths.push(cwd.join(".claude/settings.json"));
        shadow_paths.push(cwd.join(".claude/settings.local.json"));
        let canonical_settings_path = std::fs::canonicalize(&self.configured_home)
            .unwrap_or_else(|_| self.configured_home.clone())
            .join("settings.json");
        let mut seen = BTreeSet::new();
        for path in shadow_paths {
            let source = if path.ends_with(".claude/settings.local.json") {
                ShadowSource::ClaudeLocal
            } else if path.ends_with(".claude/settings.json") && path.starts_with(&cwd) {
                ShadowSource::ClaudeShared
            } else {
                ShadowSource::ClaudeManaged
            };
            let explicit_file_symlink = std::fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink());
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            if path == self.settings_path() || path == canonical_settings_path {
                if explicit_file_symlink {
                    if has_owned_shadow(&user_document, ownership) && !shadows.contains(&source) {
                        shadows.push(source);
                    }
                    collect_provider_mode_shadows(&user_document, &mut shadows);
                }
                continue;
            }
            if !seen.insert(path.clone()) || !path.exists() {
                continue;
            }
            let document = fs_read_json(&path)?;
            if has_owned_shadow(&document, ownership) && !shadows.contains(&source) {
                shadows.push(source);
            }
            collect_provider_mode_shadows(&document, &mut shadows);
        }
        Ok((snapshot, shadows))
    }

    pub(crate) fn inspect_with_ownership(
        &self,
        ownership: ClaudeConfigOwnership,
    ) -> Result<ClaudeConfigSnapshot, ClaudeProblem> {
        self.read_snapshot(ownership).map(|(snapshot, _)| snapshot)
    }

    pub fn desired_takeover(
        &self,
        model: &str,
        base_url: &str,
        routing_credential: &str,
    ) -> DesiredClaudeState {
        self.desired_takeover_v2(model, base_url, routing_credential)
    }

    pub(crate) fn desired_direct(
        &self,
        model: &str,
        base_url: &str,
        authentication: ProviderAuthentication,
        provider_credential: &str,
    ) -> Result<DesiredClaudeState, ClaudeProblem> {
        let (auth_token, api_key) = match authentication {
            ProviderAuthentication::AnthropicBearer => {
                (Some(Value::String(provider_credential.to_owned())), None)
            }
            ProviderAuthentication::AnthropicApiKey => {
                (None, Some(Value::String(provider_credential.to_owned())))
            }
            ProviderAuthentication::OpenaiBearer | ProviderAuthentication::CodexSubscription => {
                return Err(ClaudeProblem::new("invalid-provider", None));
            }
        };
        Ok(DesiredClaudeState {
            ownership_version: ClaudeConfigOwnership::FourField,
            mode: ManagedClaudeMode::Direct,
            owned: OwnedClaudeState {
                base_url: Some(Value::String(base_url.to_owned())),
                auth_token,
                model: Some(Value::String(model.to_owned())),
                api_key,
            },
        })
    }

    pub(crate) fn desired_takeover_v2(
        &self,
        model: &str,
        base_url: &str,
        routing_credential: &str,
    ) -> DesiredClaudeState {
        self.desired_takeover_with_ownership(
            model,
            base_url,
            routing_credential,
            ClaudeConfigOwnership::FourField,
        )
    }

    pub(crate) fn desired_takeover_with_ownership(
        &self,
        model: &str,
        base_url: &str,
        routing_credential: &str,
        ownership: ClaudeConfigOwnership,
    ) -> DesiredClaudeState {
        DesiredClaudeState {
            ownership_version: ownership,
            mode: ManagedClaudeMode::Takeover,
            owned: OwnedClaudeState {
                base_url: Some(Value::String(base_url.to_owned())),
                auth_token: Some(Value::String(routing_credential.to_owned())),
                model: Some(Value::String(model.to_owned())),
                api_key: None,
            },
        }
    }

    pub fn atomic_apply(
        &self,
        before: &ClaudeConfigSnapshot,
        desired: &DesiredClaudeState,
    ) -> Result<(), ClaudeProblem> {
        if before.ownership_version != desired.ownership_version {
            return Err(ClaudeProblem::new(
                "configuration-write-failed",
                Some(self.settings_path()),
            ));
        }
        self.write_owned(before, &desired.owned, false)?;
        self.verify(before, desired)
    }

    pub fn verify(
        &self,
        before: &ClaudeConfigSnapshot,
        desired: &DesiredClaudeState,
    ) -> Result<(), ClaudeProblem> {
        let current = self.inspect_with_ownership(before.ownership_version)?;
        if current.owned != desired.owned
            || current.unrelated_fingerprint != before.unrelated_fingerprint
        {
            return Err(ClaudeProblem::new(
                "configuration-write-failed",
                Some(self.settings_path()),
            ));
        }
        Ok(())
    }

    pub(crate) fn inspect_takeover(
        &self,
        desired: &DesiredClaudeState,
    ) -> Result<ClaudeConfigSnapshot, ClaudeProblem> {
        let current = self.inspect_with_ownership(desired.ownership_version)?;
        if current.owned == desired.owned {
            Ok(current)
        } else {
            Err(ClaudeProblem::new(
                "configuration-collision",
                Some(self.settings_path()),
            ))
        }
    }

    /// The expectation and its before snapshot must come from one authoritative committed
    /// Recovery Intent. Matching user-authored values without that expectation remain unmanaged.
    pub(crate) fn inspect_managed_state(
        &self,
        committed: Option<(&DesiredClaudeState, &ClaudeConfigSnapshot)>,
    ) -> Result<ManagedClaudeState, ClaudeProblem> {
        let ownership = committed
            .map(|(expected, _)| expected.ownership_version)
            .unwrap_or(ClaudeConfigOwnership::FourField);
        let current = self.inspect_with_ownership(ownership)?;
        let Some((expected, committed_before)) = committed else {
            return Ok(ManagedClaudeState::Unmanaged { snapshot: current });
        };
        if committed_before.ownership_version != ownership
            || current.owned != expected.owned
            || current.unrelated_fingerprint != committed_before.unrelated_fingerprint
        {
            return Err(ClaudeProblem::new(
                "configuration-collision",
                Some(self.settings_path()),
            ));
        }
        Ok(match expected.mode {
            ManagedClaudeMode::Direct => ManagedClaudeState::Direct { snapshot: current },
            ManagedClaudeMode::Takeover => ManagedClaudeState::Takeover { snapshot: current },
        })
    }

    pub fn restore(
        &self,
        before: &ClaudeConfigSnapshot,
        expected_current: &DesiredClaudeState,
    ) -> Result<(), ClaudeProblem> {
        if before.ownership_version != expected_current.ownership_version {
            return Err(ClaudeProblem::new(
                "recovery-required",
                Some(self.settings_path()),
            ));
        }
        let current = self.inspect_with_ownership(before.ownership_version)?;
        if current.owned != expected_current.owned {
            return Err(ClaudeProblem::new(
                "recovery-required",
                Some(self.settings_path()),
            ));
        }
        let remove_file = !before.identity.exists()
            && current
                .unrelated
                .as_ref()
                .and_then(Value::as_object)
                .is_some_and(serde_json::Map::is_empty);
        self.write_owned(&current, &before.owned, remove_file)?;
        let restored = self.inspect_with_ownership(before.ownership_version)?;
        if restored.owned != before.owned
            || restored.unrelated_fingerprint != current.unrelated_fingerprint
        {
            return Err(ClaudeProblem::new(
                "recovery-required",
                Some(self.settings_path()),
            ));
        }
        Ok(())
    }

    pub fn restore_or_confirm_before(
        &self,
        before: &ClaudeConfigSnapshot,
        expected_current: &DesiredClaudeState,
    ) -> Result<(), ClaudeProblem> {
        if self.matches_before(before) {
            return Ok(());
        }
        self.restore(before, expected_current)?;
        if self.matches_before(before) {
            Ok(())
        } else {
            Err(ClaudeProblem::new(
                "recovery-required",
                Some(self.settings_path()),
            ))
        }
    }

    pub fn preflight(
        &self,
        context: &ClaudePreflightContext,
    ) -> Result<ClaudePreflightReport, ClaudeProblem> {
        self.preflight_snapshot_with_ownership(context, None, ClaudeConfigOwnership::FourField)
            .map(|(report, _)| report)
    }

    pub(crate) fn preflight_snapshot_with_ownership(
        &self,
        context: &ClaudePreflightContext,
        expected_takeover: Option<&DesiredClaudeState>,
        ownership: ClaudeConfigOwnership,
    ) -> Result<(ClaudePreflightReport, ClaudeConfigSnapshot), ClaudeProblem> {
        let cwd = self.validate_context(context)?;
        let (snapshot, document) = self.read_snapshot(ownership)?;
        validate_provider_modes(&document, self.settings_path(), "user-settings")?;
        if expected_takeover.is_some_and(|expected| snapshot.owned != expected.owned) {
            return Err(ClaudeProblem::new(
                "configuration-collision",
                Some(self.settings_path()),
            ));
        }
        let mut shadow_paths = self.managed_settings_paths.clone();
        shadow_paths.push(cwd.join(".claude/settings.json"));
        shadow_paths.push(cwd.join(".claude/settings.local.json"));
        let canonical_settings_path = std::fs::canonicalize(&self.configured_home)
            .unwrap_or_else(|_| self.configured_home.clone())
            .join("settings.json");
        let mut seen = BTreeSet::new();
        for path in shadow_paths {
            if path == self.settings_path() || path == canonical_settings_path {
                continue;
            }
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            if !seen.insert(path.clone()) || !path.exists() {
                continue;
            }
            let source = fs_read_json(&path)?;
            let source_kind = if path.ends_with(".claude/settings.local.json") {
                "local-project-settings"
            } else if path.ends_with(".claude/settings.json") && path.starts_with(&cwd) {
                "shared-project-settings"
            } else {
                "managed-settings"
            };
            if has_owned_shadow(&source, ownership) {
                return Err(ClaudeProblem::new("shadowing-configuration", Some(&path))
                    .with_source(source_kind));
            }
            validate_provider_modes(&source, &path, source_kind)?;
        }
        Ok((
            ClaudePreflightReport {
                restart_required: true,
                unobservable_shadows: [
                    ClaudeRuntimeShadow::SettingsFlag,
                    ClaudeRuntimeShadow::ModelFlag,
                    ClaudeRuntimeShadow::InteractiveModel,
                    ClaudeRuntimeShadow::ResumedSession,
                    ClaudeRuntimeShadow::ExternalEnvironment,
                ],
            },
            snapshot,
        ))
    }

    fn validate_context(&self, context: &ClaudePreflightContext) -> Result<PathBuf, ClaudeProblem> {
        if !context.has_valid_blocking_selector() {
            return Err(ClaudeProblem::new(
                "preflight-context-required",
                Some(self.settings_path()),
            ));
        }
        let cwd = PathBuf::from(&context.cwd);
        if !cwd.is_absolute() {
            return Err(ClaudeProblem::new("preflight-context-required", None));
        }
        let cwd = std::fs::canonicalize(cwd)
            .map_err(|_| ClaudeProblem::new("preflight-context-required", None))?;
        if !cwd.is_dir() {
            return Err(ClaudeProblem::new("preflight-context-required", None));
        }
        if let Some(observed) = &context.claude_config_dir {
            let observed = PathBuf::from(observed);
            let resolved = std::fs::canonicalize(&observed).unwrap_or(observed);
            let supported = self
                .settings_path()
                .parent()
                .is_some_and(|parent| resolved == parent || resolved == self.configured_home);
            if !supported {
                return Err(ClaudeProblem::new(
                    "unsupported-configuration-home",
                    Some(&resolved),
                ));
            }
        }
        if matches!(
            context.selector_state,
            ClaudeSelectorState::Enabled | ClaudeSelectorState::UnknownNonempty
        ) || matches!(
            context.host_managed_state,
            ClaudeHostManagedState::Managed | ClaudeHostManagedState::Unknown
        ) {
            let selector = context
                .blocking_selector
                .expect("validated Claude context has an exact blocking selector");
            return Err(
                ClaudeProblem::new("provider-mode-active", Some(self.settings_path()))
                    .with_source("control-plane-context")
                    .with_selector(selector),
            );
        }
        Ok(cwd)
    }

    pub async fn reconcile_pending(&self, store: &StateStore) -> Result<(), ClaudeProblem> {
        for intent in store
            .pending_recovery_intents()
            .await
            .map_err(|_| ClaudeProblem::new("recovery-required", Some(self.settings_path())))?
        {
            if intent.target() != Target::Claude {
                continue;
            }
            self.reconcile_one(store, &intent).await?;
        }
        Ok(())
    }

    async fn reconcile_one(
        &self,
        store: &StateStore,
        intent: &RecoveryIntent,
    ) -> Result<(), ClaudeProblem> {
        let before = intent
            .claude_before()
            .ok_or_else(|| ClaudeProblem::new("recovery-required", Some(self.settings_path())))?;
        let desired = intent
            .claude_desired()
            .ok_or_else(|| ClaudeProblem::new("recovery-required", Some(self.settings_path())))?;
        if intent.config_path() != self.settings_path() {
            return self.mark_recovery_required(store, intent).await;
        }
        let current = match self.inspect_with_ownership(before.ownership_version) {
            Ok(current) => current,
            Err(_) => return self.mark_recovery_required(store, intent).await,
        };
        if current.owned == before.owned
            && current.unrelated_fingerprint == before.unrelated_fingerprint
        {
            return store
                .set_recovery_state(intent.id(), RecoveryState::RolledBack)
                .await
                .map_err(|_| ClaudeProblem::new("recovery-required", Some(self.settings_path())));
        }
        if current.owned == desired.owned
            && current.unrelated_fingerprint == before.unrelated_fingerprint
            && self.restore(before, desired).is_ok()
            && self.matches_before(before)
        {
            return store
                .set_recovery_state(intent.id(), RecoveryState::RolledBack)
                .await
                .map_err(|_| ClaudeProblem::new("recovery-required", Some(self.settings_path())));
        }
        self.mark_recovery_required(store, intent).await
    }

    async fn mark_recovery_required(
        &self,
        store: &StateStore,
        intent: &RecoveryIntent,
    ) -> Result<(), ClaudeProblem> {
        let _ = store
            .set_recovery_state(intent.id(), RecoveryState::RecoveryRequired)
            .await;
        Err(ClaudeProblem::new(
            "recovery-required",
            Some(self.settings_path()),
        ))
    }

    pub(crate) fn matches_before(&self, before: &ClaudeConfigSnapshot) -> bool {
        self.inspect_with_ownership(before.ownership_version)
            .is_ok_and(|current| {
                current.owned == before.owned
                    && current.unrelated_fingerprint == before.unrelated_fingerprint
            })
    }

    pub(crate) fn matches_pre_intent_snapshot(&self, before: &ClaudeConfigSnapshot) -> bool {
        self.inspect_with_ownership(before.ownership_version)
            .is_ok_and(|current| current == *before)
    }

    fn write_owned(
        &self,
        expected: &ClaudeConfigSnapshot,
        owned: &OwnedClaudeState,
        remove_file: bool,
    ) -> Result<(), ClaudeProblem> {
        let (current, mut document, original_bytes) =
            self.read_snapshot_with_bytes(expected.ownership_version)?;
        if current != *expected {
            return Err(ClaudeProblem::new(
                "configuration-write-failed",
                Some(self.settings_path()),
            ));
        }
        let reversible_bytes = reversible_env_edit(
            &original_bytes,
            &document,
            owned,
            expected.ownership_version,
        );
        apply_owned(&mut document, owned, expected.ownership_version);
        let bytes = match reversible_bytes {
            Some(bytes) => bytes,
            None => serde_json::to_vec_pretty(&document).map_err(|_| {
                ClaudeProblem::new("configuration-write-failed", Some(self.settings_path()))
            })?,
        };
        self.file
            .replace(&expected.identity, &bytes, remove_file)
            .map_err(|error| map_file_error(error, Some(self.settings_path())))
    }

    fn read_snapshot(
        &self,
        ownership: ClaudeConfigOwnership,
    ) -> Result<(ClaudeConfigSnapshot, Value), ClaudeProblem> {
        self.read_snapshot_with_bytes(ownership)
            .map(|(snapshot, document, _)| (snapshot, document))
    }

    fn read_snapshot_with_bytes(
        &self,
        ownership: ClaudeConfigOwnership,
    ) -> Result<(ClaudeConfigSnapshot, Value, Vec<u8>), ClaudeProblem> {
        let contents = self
            .file
            .read()
            .map_err(|error| map_file_error(error, Some(self.settings_path())))?;
        let document = if contents.identity.exists() {
            serde_json::from_slice::<Value>(&contents.bytes).map_err(|_| {
                ClaudeProblem::new("invalid-configuration", Some(self.settings_path()))
            })?
        } else {
            Value::Object(Map::new())
        };
        if !document.is_object() {
            return Err(ClaudeProblem::new(
                "invalid-configuration",
                Some(self.settings_path()),
            ));
        }
        if document.get("env").is_some_and(|value| !value.is_object()) {
            return Err(ClaudeProblem::new(
                "configuration-collision",
                Some(self.settings_path()),
            ));
        }
        let owned = capture_owned(&document, ownership);
        let unrelated = unrelated_projection(&document, ownership);
        let unrelated_fingerprint = semantic_fingerprint(&unrelated)
            .map_err(|_| ClaudeProblem::new("invalid-configuration", Some(self.settings_path())))?;
        Ok((
            ClaudeConfigSnapshot {
                ownership_version: ownership,
                identity: contents.identity,
                owned,
                unrelated_fingerprint,
                unrelated: Some(unrelated),
            },
            document,
            contents.bytes,
        ))
    }
}

fn reversible_env_edit(
    original: &[u8],
    document: &Value,
    desired: &OwnedClaudeState,
    ownership: ClaudeConfigOwnership,
) -> Option<Vec<u8>> {
    let object = document.as_object()?;
    let desired_has_owned = owned_has_values(desired, ownership);
    match object.get("env") {
        None if desired_has_owned => {
            insert_managed_env(original, object.is_empty(), desired, ownership)
        }
        Some(Value::Object(env))
            if !desired_has_owned
                && !env.is_empty()
                && env
                    .keys()
                    .all(|key| ownership.owned_keys().contains(&key.as_str())) =>
        {
            remove_inserted_managed_env(original, object.len() == 1)
        }
        _ => None,
    }
}

fn owned_has_values(owned: &OwnedClaudeState, ownership: ClaudeConfigOwnership) -> bool {
    owned.base_url.is_some()
        || owned.auth_token.is_some()
        || owned.model.is_some()
        || (ownership == ClaudeConfigOwnership::FourField && owned.api_key.is_some())
}

fn insert_managed_env(
    original: &[u8],
    root_is_empty: bool,
    desired: &OwnedClaudeState,
    ownership: ClaudeConfigOwnership,
) -> Option<Vec<u8>> {
    let (open, close) = root_object_bounds(original)?;
    let mut managed = Value::Object(Map::new());
    apply_owned(&mut managed, desired, ownership);
    let env = serde_json::to_vec(managed.get("env")?).ok()?;
    let mut member = Vec::with_capacity(env.len() + 8);
    if !root_is_empty {
        member.push(b',');
    }
    member.extend_from_slice(br#""env":"#);
    member.extend_from_slice(&env);
    let insertion = if root_is_empty { open + 1 } else { close };
    let mut bytes = Vec::with_capacity(original.len() + member.len());
    bytes.extend_from_slice(&original[..insertion]);
    bytes.extend_from_slice(&member);
    bytes.extend_from_slice(&original[insertion..]);
    Some(bytes)
}

fn remove_inserted_managed_env(original: &[u8], root_only_env: bool) -> Option<Vec<u8>> {
    let (open, close) = root_object_bounds(original)?;
    let (remove_start, value_start) = if root_only_env {
        let member_start = skip_json_whitespace(original, open + 1);
        let value_start = exact_env_value_start(original, member_start)?;
        (member_start, value_start)
    } else {
        let member = br#","env":"#;
        let relative = find_last_subslice(&original[open + 1..close], member)?;
        let member_start = open + 1 + relative;
        let value_start = exact_env_value_start(original, member_start + 1)?;
        (member_start, value_start)
    };
    let value_end = json_value_end(original, value_start)?;
    if original[value_end..close]
        .iter()
        .any(|byte| !byte.is_ascii_whitespace())
    {
        return None;
    }
    let mut bytes = Vec::with_capacity(original.len() - (value_end - remove_start));
    bytes.extend_from_slice(&original[..remove_start]);
    bytes.extend_from_slice(&original[value_end..]);
    Some(bytes)
}

fn root_object_bounds(bytes: &[u8]) -> Option<(usize, usize)> {
    let open = bytes.iter().position(|byte| !byte.is_ascii_whitespace())?;
    let close = bytes.iter().rposition(|byte| !byte.is_ascii_whitespace())?;
    (bytes.get(open) == Some(&b'{') && bytes.get(close) == Some(&b'}')).then_some((open, close))
}

fn exact_env_value_start(bytes: &[u8], member_start: usize) -> Option<usize> {
    let mut cursor = member_start;
    if bytes.get(cursor) != Some(&b'"') {
        return None;
    }
    cursor += 1;
    if bytes.get(cursor..cursor + 3)? != b"env" {
        return None;
    }
    cursor += 3;
    if bytes.get(cursor) != Some(&b'"') {
        return None;
    }
    cursor = skip_json_whitespace(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b':') {
        return None;
    }
    Some(skip_json_whitespace(bytes, cursor + 1))
}

fn skip_json_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    cursor
}

fn json_value_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut stream = serde_json::Deserializer::from_slice(bytes.get(start..)?).into_iter::<Value>();
    stream.next()?.ok()?;
    Some(start + stream.byte_offset())
}

fn find_last_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

fn semantic_fingerprint(value: &impl Serialize) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(value)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let mut encoded = String::with_capacity(digest.as_ref().len() * 2);
    for byte in digest.as_ref() {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(encoded)
}

fn capture_owned(document: &Value, ownership: ClaudeConfigOwnership) -> OwnedClaudeState {
    let env = document.get("env").and_then(Value::as_object);
    OwnedClaudeState {
        base_url: env.and_then(|env| env.get(BASE_URL_KEY)).cloned(),
        auth_token: env.and_then(|env| env.get(AUTH_TOKEN_KEY)).cloned(),
        model: env.and_then(|env| env.get(MODEL_KEY)).cloned(),
        api_key: matches!(ownership, ClaudeConfigOwnership::FourField)
            .then(|| env.and_then(|env| env.get(API_KEY)).cloned())
            .flatten(),
    }
}

fn unrelated_projection(document: &Value, ownership: ClaudeConfigOwnership) -> Value {
    let mut unrelated = document.clone();
    let object = unrelated
        .as_object_mut()
        .expect("validated Claude settings object");
    if let Some(env) = object.get_mut("env").and_then(Value::as_object_mut) {
        for key in ownership.owned_keys() {
            env.remove(*key);
        }
        if env.is_empty() {
            object.remove("env");
        }
    }
    unrelated
}

fn apply_owned(document: &mut Value, owned: &OwnedClaudeState, ownership: ClaudeConfigOwnership) {
    let object = document
        .as_object_mut()
        .expect("validated Claude settings object");
    let needs_env = owned.base_url.is_some()
        || owned.auth_token.is_some()
        || owned.model.is_some()
        || (ownership == ClaudeConfigOwnership::FourField && owned.api_key.is_some());
    if needs_env {
        object
            .entry("env")
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if let Some(env) = object.get_mut("env").and_then(Value::as_object_mut) {
        for (key, value) in [
            (BASE_URL_KEY, &owned.base_url),
            (AUTH_TOKEN_KEY, &owned.auth_token),
            (MODEL_KEY, &owned.model),
        ] {
            match value {
                Some(value) => {
                    env.insert(key.to_owned(), value.clone());
                }
                None => {
                    env.remove(key);
                }
            }
        }
        if ownership == ClaudeConfigOwnership::FourField {
            match &owned.api_key {
                Some(value) => {
                    env.insert(API_KEY.to_owned(), value.clone());
                }
                None => {
                    env.remove(API_KEY);
                }
            }
        }
        if env.is_empty() {
            object.remove("env");
        }
    }
}

fn validate_provider_modes(
    document: &Value,
    path: &Path,
    source: &'static str,
) -> Result<(), ClaudeProblem> {
    let Some(env) = document.get("env").and_then(Value::as_object) else {
        return Ok(());
    };
    for selector in PROVIDER_SELECTORS
        .into_iter()
        .chain([ClaudeBlockingSelector::HostManaged])
    {
        if env.get(selector.as_str()).is_some_and(selector_blocks) {
            return Err(ClaudeProblem::new("provider-mode-active", Some(path))
                .with_source(source)
                .with_selector(selector));
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn collect_provider_mode_shadows(document: &Value, shadows: &mut Vec<ShadowSource>) {
    let Some(env) = document.get("env").and_then(Value::as_object) else {
        return;
    };
    for selector in PROVIDER_SELECTORS
        .into_iter()
        .chain([ClaudeBlockingSelector::HostManaged])
    {
        if !env.get(selector.as_str()).is_some_and(selector_blocks) {
            continue;
        }
        let source = if selector == ClaudeBlockingSelector::HostManaged {
            ShadowSource::ClaudeHostManaged
        } else {
            ShadowSource::ClaudeSelector(selector)
        };
        if !shadows.contains(&source) {
            shadows.push(source);
        }
    }
}

fn selector_blocks(value: &Value) -> bool {
    match value {
        Value::Bool(false) => false,
        Value::Number(number) if number.as_i64() == Some(0) => false,
        Value::String(value)
            if value.is_empty() || value.eq_ignore_ascii_case("false") || value == "0" =>
        {
            false
        }
        Value::Null => false,
        _ => true,
    }
}

fn has_owned_shadow(document: &Value, ownership: ClaudeConfigOwnership) -> bool {
    document
        .get("env")
        .and_then(Value::as_object)
        .is_some_and(|env| {
            ownership
                .owned_keys()
                .iter()
                .any(|key| env.contains_key(*key))
        })
}

fn fs_read_json(path: &Path) -> Result<Value, ClaudeProblem> {
    let bytes =
        std::fs::read(path).map_err(|_| ClaudeProblem::new("invalid-configuration", Some(path)))?;
    let document = serde_json::from_slice::<Value>(&bytes)
        .map_err(|_| ClaudeProblem::new("invalid-configuration", Some(path)))?;
    if !document.is_object() || document.get("env").is_some_and(|value| !value.is_object()) {
        return Err(ClaudeProblem::new("invalid-configuration", Some(path)));
    }
    Ok(document)
}

fn default_managed_settings_paths(user_home: &Path) -> Vec<PathBuf> {
    if std::env::var_os("HOME").as_deref() != Some(user_home.as_os_str()) {
        return Vec::new();
    }
    #[cfg(target_os = "macos")]
    {
        vec![PathBuf::from(
            "/Library/Application Support/ClaudeCode/managed-settings.json",
        )]
    }
    #[cfg(target_os = "linux")]
    {
        vec![PathBuf::from("/etc/claude-code/managed-settings.json")]
    }
}

fn map_file_error(error: ManagedFileError, path: Option<&Path>) -> ClaudeProblem {
    let code = match error {
        ManagedFileError::WriteFailed => "configuration-write-failed",
        ManagedFileError::RecoveryRequired => "recovery-required",
    };
    ClaudeProblem::new(code, path)
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Mutex};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;
    use crate::control::protocol::ProviderAuthentication;

    fn fixture(source: &str) -> (TempDir, ClaudeConfigCodec) {
        let home = TempDir::new().expect("temporary home");
        let codec = ClaudeConfigCodec::for_user_home(home.path()).expect("Claude codec");
        fs::create_dir_all(codec.settings_path().parent().expect("settings parent"))
            .expect("create Claude home");
        fs::write(codec.settings_path(), source).expect("write settings fixture");
        (home, codec)
    }

    fn read_settings(codec: &ClaudeConfigCodec) -> Value {
        serde_json::from_slice(&fs::read(codec.settings_path()).expect("read settings"))
            .expect("parse settings")
    }

    fn secret_json_string_matches(value: &Value, expected: &str) -> Result<(), &'static str> {
        if value.as_str() == Some(expected) {
            Ok(())
        } else {
            Err("secret JSON value mismatch")
        }
    }

    #[test]
    fn direct_bearer_sets_auth_token_removes_api_key_and_preserves_unrelated_semantics() {
        let source = serde_json::json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://prior.example",
                "ANTHROPIC_MODEL": "prior-model",
                "ANTHROPIC_AUTH_TOKEN": "prior-auth-secret",
                "ANTHROPIC_API_KEY": "prior-api-secret",
                "OPERATOR_FLAG": {"nested": [1, true, null]}
            },
            "model": "operator-top-level-model",
            "permissions": {"allow": ["Read"]}
        });
        let (_home, codec) = fixture(&source.to_string());
        #[cfg(unix)]
        fs::set_permissions(codec.settings_path(), fs::Permissions::from_mode(0o640))
            .expect("set fixture mode");
        let before = codec.inspect().expect("inspect settings");
        let desired = codec
            .desired_direct(
                "claude-direct-model",
                "https://direct.example",
                ProviderAuthentication::AnthropicBearer,
                "direct-bearer-secret",
            )
            .expect("valid bearer Direct state");

        codec
            .atomic_apply(&before, &desired)
            .expect("apply bearer Direct state");

        let after = read_settings(&codec);
        assert_eq!(after["env"][BASE_URL_KEY], "https://direct.example");
        assert_eq!(after["env"][MODEL_KEY], "claude-direct-model");
        secret_json_string_matches(&after["env"][AUTH_TOKEN_KEY], "direct-bearer-secret").unwrap();
        assert!(after["env"].get(API_KEY).is_none());
        assert_eq!(
            after["env"]["OPERATOR_FLAG"],
            source["env"]["OPERATOR_FLAG"]
        );
        assert_eq!(after["model"], source["model"]);
        assert_eq!(after["permissions"], source["permissions"]);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(codec.settings_path())
                .expect("settings metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    #[test]
    fn direct_api_key_restores_both_prior_credentials_and_preserves_later_unrelated_edits() {
        let source = serde_json::json!({
            "env": {
                "ANTHROPIC_AUTH_TOKEN": "prior-auth-secret",
                "ANTHROPIC_API_KEY": "prior-api-secret",
                "OPERATOR_FLAG": "keep"
            }
        });
        let (_home, codec) = fixture(&source.to_string());
        let before = codec.inspect().expect("inspect settings");
        let desired = codec
            .desired_direct(
                "claude-api-model",
                "https://api.example",
                ProviderAuthentication::AnthropicApiKey,
                "direct-api-secret",
            )
            .expect("valid API-key Direct state");

        codec
            .atomic_apply(&before, &desired)
            .expect("apply API-key Direct state");
        let mut live = read_settings(&codec);
        assert!(live["env"].get(AUTH_TOKEN_KEY).is_none());
        secret_json_string_matches(&live["env"][API_KEY], "direct-api-secret").unwrap();
        live["operatorAfterApply"] = serde_json::json!({"keep": [1, 2, 3]});
        fs::write(
            codec.settings_path(),
            serde_json::to_vec_pretty(&live).expect("serialize settings"),
        )
        .expect("write unrelated edit");

        codec
            .restore(&before, &desired)
            .expect("restore prior state");

        let restored = read_settings(&codec);
        secret_json_string_matches(&restored["env"][AUTH_TOKEN_KEY], "prior-auth-secret").unwrap();
        secret_json_string_matches(&restored["env"][API_KEY], "prior-api-secret").unwrap();
        assert_eq!(restored["env"]["OPERATOR_FLAG"], "keep");
        assert_eq!(
            restored["operatorAfterApply"],
            serde_json::json!({"keep": [1, 2, 3]})
        );
    }

    #[test]
    fn direct_diagnostics_are_fixed_and_secret_free() {
        let (_home, codec) = fixture(r#"{"env":{"ANTHROPIC_API_KEY":"prior-92031"}}"#);
        let before = codec.inspect().expect("inspect settings");
        let desired = codec
            .desired_direct(
                "model-83017",
                "https://base-64109.example",
                ProviderAuthentication::AnthropicBearer,
                "credential-75193",
            )
            .expect("valid Direct state");
        codec
            .atomic_apply(&before, &desired)
            .expect("apply Direct state");
        fs::write(
            codec.settings_path(),
            r#"{"env":{"ANTHROPIC_MODEL":48127}}"#,
        )
        .expect("write drifted settings");

        let error = codec
            .restore(&before, &desired)
            .expect_err("owned drift must block restore");
        let diagnostics = format!("{error:?}\n{error}\n{desired:?}\n{before:?}");
        for forbidden in [
            "prior-92031",
            "model-83017",
            "base-64109",
            "credential-75193",
            "48127",
            "112, 114, 105, 111, 114",
        ] {
            assert!(
                !diagnostics.contains(forbidden),
                "Claude diagnostic exposed controlled secret material"
            );
        }
        assert_eq!(error.code(), "recovery-required");
    }

    #[test]
    fn direct_rejects_non_claude_authentication_without_writing() {
        let (_home, codec) = fixture(r#"{"operator":"keep"}"#);
        let error = codec
            .desired_direct(
                "model",
                "https://base.example",
                ProviderAuthentication::OpenaiBearer,
                "credential-secret",
            )
            .expect_err("Codex authentication must be rejected");

        let diagnostic = format!("{error:?}\n{error}");
        assert!(!diagnostic.contains("credential-secret"));
        assert_eq!(error.code(), "invalid-provider");
        assert_eq!(read_settings(&codec)["operator"], "keep");
    }

    #[test]
    fn managed_state_requires_a_caller_supplied_committed_expectation() {
        let (_home, codec) = fixture(r#"{"env":{"OPERATOR_FLAG":"keep"}}"#);
        let committed_before = codec.inspect().expect("inspect unmanaged settings");
        let desired = codec
            .desired_direct(
                "model",
                "https://base.example",
                ProviderAuthentication::AnthropicBearer,
                "credential-secret",
            )
            .expect("valid Direct state");
        codec
            .atomic_apply(&committed_before, &desired)
            .expect("apply Direct state");

        assert!(matches!(
            codec
                .inspect_managed_state(None)
                .expect("inspect without ownership"),
            ManagedClaudeState::Unmanaged { .. }
        ));
        assert!(matches!(
            codec
                .inspect_managed_state(Some((&desired, &committed_before)))
                .expect("inspect committed Direct"),
            ManagedClaudeState::Direct { .. }
        ));
    }

    #[test]
    fn managed_state_reports_every_direct_and_takeover_drift_as_collision() {
        #[derive(Clone, Copy, Debug)]
        enum Mode {
            Direct,
            Takeover,
        }

        fn mutate_base_url(document: &mut Value) {
            document["env"][BASE_URL_KEY] =
                Value::String("https://drift-base-91827.example".into());
        }
        fn mutate_model(document: &mut Value) {
            document["env"][MODEL_KEY] = Value::String("drift-model-82917".into());
        }
        fn mutate_auth_token(document: &mut Value) {
            document["env"][AUTH_TOKEN_KEY] = Value::String("drift-auth-73129".into());
        }
        fn mutate_api_key(document: &mut Value) {
            document["env"][API_KEY] = Value::String("drift-api-64219".into());
        }
        fn mutate_unrelated(document: &mut Value) {
            document["operator"] = serde_json::json!({"nested": [53921, true]});
        }

        type DriftMutation = (&'static str, fn(&mut Value));
        let mutations: [DriftMutation; 5] = [
            ("base-url", mutate_base_url),
            ("model", mutate_model),
            ("auth-token", mutate_auth_token),
            ("api-key", mutate_api_key),
            ("unrelated", mutate_unrelated),
        ];
        for mode in [Mode::Direct, Mode::Takeover] {
            for (field, mutate) in mutations {
                let (_home, codec) = fixture(r#"{"operator":{"nested":[1,true]}}"#);
                let committed_before = codec.inspect().expect("inspect unmanaged settings");
                let desired = match mode {
                    Mode::Direct => codec
                        .desired_direct(
                            "direct-model-35719",
                            "https://direct-base-46831.example",
                            ProviderAuthentication::AnthropicBearer,
                            "direct-credential-24691",
                        )
                        .expect("valid Direct state"),
                    Mode::Takeover => codec.desired_takeover_v2(
                        "takeover-model-17539",
                        "http://127.0.0.1:43124",
                        "routing-credential-86413",
                    ),
                };
                codec
                    .atomic_apply(&committed_before, &desired)
                    .expect("apply committed state");
                let observed = codec
                    .inspect_managed_state(Some((&desired, &committed_before)))
                    .expect("inspect exact committed state");
                assert!(matches!(
                    (mode, observed),
                    (Mode::Direct, ManagedClaudeState::Direct { .. })
                        | (Mode::Takeover, ManagedClaudeState::Takeover { .. })
                ));
                let mut live = read_settings(&codec);
                mutate(&mut live);
                fs::write(
                    codec.settings_path(),
                    serde_json::to_vec_pretty(&live).expect("serialize drift"),
                )
                .expect("write drift");

                let error = match codec.inspect_managed_state(Some((&desired, &committed_before))) {
                    Ok(state) => {
                        panic!("{mode:?} {field} committed drift was accepted: {state:?}")
                    }
                    Err(error) => error,
                };
                let diagnostic = format!("{error:?}\n{error}");
                for forbidden in [
                    "direct-credential-24691",
                    "routing-credential-86413",
                    "drift-auth-73129",
                    "drift-api-64219",
                    "24691",
                    "86413",
                    "91827",
                    "82917",
                    "73129",
                    "64219",
                    "35719",
                    "46831",
                    "17539",
                    "53921",
                    "100, 105, 114, 101, 99, 116",
                ] {
                    assert!(
                        !diagnostic.contains(forbidden),
                        "{field} drift diagnostic exposed controlled secret material"
                    );
                }
                // The activation adapter maps this stable codec collision to recovery-required.
                assert_eq!(error.code(), "configuration-collision");
            }
        }
    }

    #[test]
    fn direct_profile_transitions_and_takeover_change_only_four_approved_fields() {
        let source = serde_json::json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://prior.example",
                "ANTHROPIC_MODEL": "prior-model",
                "ANTHROPIC_AUTH_TOKEN": "prior-auth",
                "ANTHROPIC_API_KEY": "prior-api",
                "OPERATOR_FLAG": {"keep": true}
            },
            "permissions": {"deny": ["Write"]},
            "model": "top-level-model"
        });
        let (_home, codec) = fixture(&source.to_string());
        let original = codec.inspect().expect("capture original state");
        let bearer = codec
            .desired_direct(
                "bearer-model",
                "https://bearer.example",
                ProviderAuthentication::AnthropicBearer,
                "bearer-secret",
            )
            .expect("valid bearer Direct state");
        codec
            .atomic_apply(&original, &bearer)
            .expect("apply bearer Direct state");
        let bearer_before = codec.inspect().expect("capture bearer Direct state");
        let api_key = codec
            .desired_direct(
                "api-model",
                "https://api.example",
                ProviderAuthentication::AnthropicApiKey,
                "api-secret",
            )
            .expect("valid API-key Direct state");
        codec
            .atomic_apply(&bearer_before, &api_key)
            .expect("apply API-key Direct state");
        let api_before = codec.inspect().expect("capture API-key Direct state");
        let takeover =
            codec.desired_takeover_v2("takeover-model", "http://127.0.0.1:43124", "routing-secret");
        codec
            .atomic_apply(&api_before, &takeover)
            .expect("apply Takeover state");

        let active = read_settings(&codec);
        assert_eq!(active["env"][BASE_URL_KEY], "http://127.0.0.1:43124");
        assert_eq!(active["env"][MODEL_KEY], "takeover-model");
        secret_json_string_matches(&active["env"][AUTH_TOKEN_KEY], "routing-secret").unwrap();
        assert!(active["env"].get(API_KEY).is_none());
        assert_eq!(
            active["env"]["OPERATOR_FLAG"],
            source["env"]["OPERATOR_FLAG"]
        );
        assert_eq!(active["permissions"], source["permissions"]);
        assert_eq!(active["model"], source["model"]);

        codec
            .restore(&api_before, &takeover)
            .expect("restore API-key Direct state");
        codec
            .restore(&bearer_before, &api_key)
            .expect("restore bearer Direct state");
        codec
            .restore(&original, &bearer)
            .expect("restore original state");
        let restored = read_settings(&codec);
        secret_json_string_matches(&restored["env"][AUTH_TOKEN_KEY], "prior-auth").unwrap();
        secret_json_string_matches(&restored["env"][API_KEY], "prior-api").unwrap();
        assert_eq!(
            restored["env"]["OPERATOR_FLAG"],
            source["env"]["OPERATOR_FLAG"]
        );
        assert_eq!(restored["permissions"], source["permissions"]);
        assert_eq!(restored["model"], source["model"]);
    }

    #[test]
    fn legacy_takeover_keeps_api_key_unrelated_and_v2_transition_can_restore_it() {
        let (_home, codec) =
            fixture(r#"{"env":{"ANTHROPIC_API_KEY":"legacy-api","OPERATOR_FLAG":"keep"}}"#);
        let legacy_before = codec
            .inspect_with_ownership(ClaudeConfigOwnership::LegacyThree)
            .expect("capture legacy state");
        let legacy_takeover = codec.desired_takeover_with_ownership(
            "legacy-model",
            "http://127.0.0.1:43124",
            "legacy-routing-secret",
            ClaudeConfigOwnership::LegacyThree,
        );
        codec
            .atomic_apply(&legacy_before, &legacy_takeover)
            .expect("apply legacy Takeover");
        secret_json_string_matches(&read_settings(&codec)["env"][API_KEY], "legacy-api").unwrap();
        assert!(matches!(
            codec
                .inspect_managed_state(Some((&legacy_takeover, &legacy_before)))
                .expect("validate committed legacy Takeover"),
            ManagedClaudeState::Takeover { .. }
        ));

        let v2_before = codec
            .inspect()
            .expect("capture four-field transition state");
        let v2_takeover = codec.desired_takeover_v2(
            "current-model",
            "http://127.0.0.1:43125",
            "current-routing-secret",
        );
        codec
            .atomic_apply(&v2_before, &v2_takeover)
            .expect("apply four-field Takeover");
        assert!(read_settings(&codec)["env"].get(API_KEY).is_none());

        codec
            .restore(&v2_before, &v2_takeover)
            .expect("restore legacy API key after failed upgrade");
        secret_json_string_matches(&read_settings(&codec)["env"][API_KEY], "legacy-api").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn direct_absent_file_is_private_under_restrictive_umask() {
        let home = TempDir::new().expect("temporary home");
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("--ignored")
            .arg("--exact")
            .arg("claude::config::tests::direct_umask_subprocess_helper")
            .env("MUXVIA_DIRECT_UMASK_TEST_HOME", home.path())
            .status()
            .expect("run umask helper");

        assert!(status.success());
        assert_eq!(
            fs::metadata(home.path().join(".claude/settings.json"))
                .expect("settings metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "invoked in an isolated subprocess by the Direct umask regression test"]
    fn direct_umask_subprocess_helper() {
        static UMASK_LOCK: Mutex<()> = Mutex::new(());
        struct RestoreUmask(libc::mode_t);
        impl Drop for RestoreUmask {
            fn drop(&mut self) {
                unsafe { libc::umask(self.0) };
            }
        }

        let _guard = UMASK_LOCK.lock().expect("umask lock");
        let home =
            Path::new(&std::env::var_os("MUXVIA_DIRECT_UMASK_TEST_HOME").expect("helper home"))
                .to_owned();
        let codec = ClaudeConfigCodec::for_user_home(&home).expect("Claude codec");
        let before = codec.inspect().expect("inspect absent settings");
        let desired = codec
            .desired_direct(
                "model",
                "https://base.example",
                ProviderAuthentication::AnthropicBearer,
                "credential",
            )
            .expect("valid Direct state");
        let previous = unsafe { libc::umask(0o077) };
        let _restore = RestoreUmask(previous);
        codec
            .atomic_apply(&before, &desired)
            .expect("apply Direct state");
    }
}
