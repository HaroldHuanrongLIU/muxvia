use std::{
    collections::BTreeSet,
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize, de::Error as _};
use serde_json::{Map, Value};

use super::ClaudeProblem;
use crate::{
    config::managed_file::{FileIdentity, ManagedFile, ManagedFileError, PreRenameHook},
    control::protocol::{
        ClaudeBlockingSelector, ClaudeHostManagedState, ClaudePreflightContext,
        ClaudeSelectorState, Target,
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
        self.validate_context(context)?;
        let (snapshot, document) = self.read_snapshot(ownership)?;
        validate_provider_modes(&document, self.settings_path(), "user-settings")?;
        if expected_takeover.is_some_and(|expected| snapshot.owned != expected.owned) {
            return Err(ClaudeProblem::new(
                "configuration-collision",
                Some(self.settings_path()),
            ));
        }
        let mut shadow_paths = self.managed_settings_paths.clone();
        let cwd = PathBuf::from(&context.cwd);
        shadow_paths.push(cwd.join(".claude/settings.json"));
        shadow_paths.push(cwd.join(".claude/settings.local.json"));
        let mut seen = BTreeSet::new();
        for path in shadow_paths {
            if path == self.settings_path() || !seen.insert(path.clone()) || !path.exists() {
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

    fn validate_context(&self, context: &ClaudePreflightContext) -> Result<(), ClaudeProblem> {
        if !context.has_valid_blocking_selector() {
            return Err(ClaudeProblem::new(
                "preflight-context-required",
                Some(self.settings_path()),
            ));
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
        Ok(())
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

    fn matches_before(&self, before: &ClaudeConfigSnapshot) -> bool {
        self.inspect_with_ownership(before.ownership_version)
            .is_ok_and(|current| {
                current.owned == before.owned
                    && current.unrelated_fingerprint == before.unrelated_fingerprint
            })
    }

    fn write_owned(
        &self,
        expected: &ClaudeConfigSnapshot,
        owned: &OwnedClaudeState,
        remove_file: bool,
    ) -> Result<(), ClaudeProblem> {
        let (current, mut document) = self.read_snapshot(expected.ownership_version)?;
        if current != *expected {
            return Err(ClaudeProblem::new(
                "configuration-write-failed",
                Some(self.settings_path()),
            ));
        }
        apply_owned(&mut document, owned, expected.ownership_version);
        let bytes = serde_json::to_vec_pretty(&document).map_err(|_| {
            ClaudeProblem::new("configuration-write-failed", Some(self.settings_path()))
        })?;
        self.file
            .replace(&expected.identity, &bytes, remove_file)
            .map_err(|error| map_file_error(error, Some(self.settings_path())))
    }

    fn read_snapshot(
        &self,
        ownership: ClaudeConfigOwnership,
    ) -> Result<(ClaudeConfigSnapshot, Value), ClaudeProblem> {
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
        ))
    }
}

fn semantic_fingerprint(unrelated: &Value) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(unrelated)?;
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
