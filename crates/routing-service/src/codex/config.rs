use std::{fmt, path::Path};

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, value};

use super::CodexProblem;
use crate::{
    config::managed_file::{ExchangeHook, ManagedFile, ManagedFileError, PreRenameHook},
    state::{RecoveryIntent, RecoveryState, StateStore},
};

pub use crate::config::managed_file::FileIdentity;

#[derive(Clone, Serialize, Deserialize)]
pub struct OwnedCodexState {
    #[serde(default = "default_owned_provider_key")]
    owned_provider_key: String,
    model: Option<OwnedItem>,
    model_provider: Option<OwnedItem>,
    provider_name: Option<OwnedItem>,
    provider_base_url: Option<OwnedItem>,
    provider_wire_api: Option<OwnedItem>,
    provider_http_headers: Option<OwnedItem>,
    provider_supports_websockets: Option<OwnedItem>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum CodexProviderRestoreState {
    Absent {
        key: String,
    },
    Present {
        key: String,
        #[serde(flatten)]
        fields: Box<CodexProviderRestoreFields>,
    },
    Unrepresentable {
        key: String,
    },
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CodexProviderRestoreFields {
    name: Option<OwnedItem>,
    base_url: Option<OwnedItem>,
    wire_api: Option<OwnedItem>,
    http_headers: Option<OwnedItem>,
    supports_websockets: Option<OwnedItem>,
}

impl fmt::Debug for CodexProviderRestoreState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent { .. } => formatter.write_str("CodexProviderRestoreState::Absent"),
            Self::Present { .. } => {
                formatter.write_str("CodexProviderRestoreState::Present(<redacted>)")
            }
            Self::Unrepresentable { .. } => {
                formatter.write_str("CodexProviderRestoreState::Unrepresentable")
            }
        }
    }
}

impl CodexProviderRestoreState {
    fn key(&self) -> &str {
        match self {
            Self::Absent { key } | Self::Present { key, .. } | Self::Unrepresentable { key } => key,
        }
    }

    fn is_unrepresentable(&self) -> bool {
        matches!(self, Self::Unrepresentable { .. })
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct OwnedItem {
    rendered: String,
    semantic: serde_json::Value,
}

impl fmt::Debug for OwnedCodexState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnedCodexState")
            .field("owned_provider_key", &"<redacted>")
            .field("model_present", &self.model.is_some())
            .field("model_provider_present", &self.model_provider.is_some())
            .field("provider_name_present", &self.provider_name.is_some())
            .field(
                "provider_base_url_present",
                &self.provider_base_url.is_some(),
            )
            .field(
                "provider_wire_api_present",
                &self.provider_wire_api.is_some(),
            )
            .field(
                "provider_http_headers_present",
                &self.provider_http_headers.is_some(),
            )
            .field(
                "provider_supports_websockets_present",
                &self.provider_supports_websockets.is_some(),
            )
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DesiredCodexState {
    owned: OwnedCodexState,
    #[serde(skip)]
    mode: Option<ManagedCodexMode>,
}

impl PartialEq for DesiredCodexState {
    fn eq(&self, other: &Self) -> bool {
        self.owned == other.owned
    }
}

impl Eq for DesiredCodexState {}

impl fmt::Debug for DesiredCodexState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DesiredCodexState(<redacted>)")
    }
}

impl DesiredCodexState {
    #[allow(dead_code)]
    pub(crate) fn mode(&self) -> Option<ManagedCodexMode> {
        self.mode
    }

    pub(crate) fn reconciliation_provider(&self) -> Option<(String, String, String, SecretString)> {
        let model = self.owned.model.as_ref()?.semantic.as_str()?.to_owned();
        if model.trim().is_empty() {
            return None;
        }
        let provider_key = self.owned.model_provider.as_ref()?.semantic.as_str()?;
        if provider_key.trim().is_empty() {
            return None;
        }
        let name = self
            .owned
            .provider_name
            .as_ref()?
            .semantic
            .as_str()?
            .to_owned();
        if name.trim().is_empty()
            || self.owned.provider_wire_api.as_ref()?.semantic.as_str()? != "responses"
            || self
                .owned
                .provider_supports_websockets
                .as_ref()?
                .semantic
                .as_bool()?
        {
            return None;
        }
        let base_url = self
            .owned
            .provider_base_url
            .as_ref()?
            .semantic
            .as_str()?
            .to_owned();
        if base_url.trim().is_empty() {
            return None;
        }
        let headers = self
            .owned
            .provider_http_headers
            .as_ref()?
            .semantic
            .as_object()?;
        if headers.len() != 1 {
            return None;
        }
        let credential = headers
            .get("Authorization")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| value.strip_prefix("Bearer "))?;
        if credential.trim().is_empty() {
            return None;
        }
        Some((
            name,
            model,
            base_url,
            SecretString::from(credential.to_owned()),
        ))
    }

    pub(crate) fn uses_routing_credential_header(&self) -> bool {
        self.owned
            .provider_http_headers
            .as_ref()
            .and_then(|item| item.semantic.as_object())
            .is_some_and(|headers| headers.contains_key("X-Muxvia-Routing-Credential"))
    }

    fn with_provider_ownership(mut self, ownership: &Self) -> Self {
        let provider_key = ownership.owned.effective_provider_key();
        if !provider_key.is_empty() {
            self.owned.owned_provider_key = provider_key.to_owned();
            self.owned.model_provider = desired_item(value(provider_key));
            self.owned.provider_name = ownership.owned.provider_name.clone();
        }
        self
    }
}

fn default_owned_provider_key() -> String {
    "muxvia_codex".to_owned()
}

impl OwnedCodexState {
    fn effective_provider_key(&self) -> &str {
        &self.owned_provider_key
    }
}

impl PartialEq for OwnedCodexState {
    fn eq(&self, other: &Self) -> bool {
        self.effective_provider_key() == other.effective_provider_key()
            && self.model == other.model
            && self.model_provider == other.model_provider
            && self.provider_name == other.provider_name
            && self.provider_base_url == other.provider_base_url
            && self.provider_wire_api == other.provider_wire_api
            && self.provider_http_headers == other.provider_http_headers
            && self.provider_supports_websockets == other.provider_supports_websockets
    }
}

impl Eq for OwnedCodexState {}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigSnapshot {
    identity: FileIdentity,
    owned: OwnedCodexState,
    unrelated: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_restore: Option<CodexProviderRestoreState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedCodexMode {
    Direct,
    Takeover,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ManagedCodexState {
    Unmanaged { snapshot: ConfigSnapshot },
    Direct { snapshot: ConfigSnapshot },
    Takeover { snapshot: ConfigSnapshot },
}

impl ManagedCodexState {
    fn into_snapshot(self) -> ConfigSnapshot {
        match self {
            Self::Unmanaged { snapshot }
            | Self::Direct { snapshot }
            | Self::Takeover { snapshot } => snapshot,
        }
    }
}

impl fmt::Debug for ConfigSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigSnapshot")
            .field("identity", &self.identity)
            .field("owned", &self.owned)
            .field("unrelated", &"<semantic-tree>")
            .field("provider_restore", &self.provider_restore)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct CodexObservedDocument {
    identity: FileIdentity,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CodexInstalledFileState {
    exists: bool,
    mode: Option<u32>,
}

impl fmt::Debug for CodexObservedDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexObservedDocument(<redacted>)")
    }
}

impl CodexObservedDocument {
    pub(crate) fn planned_restore_union_file_state(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
        provider_restore: &CodexProviderRestoreState,
        historical_before: &ConfigSnapshot,
    ) -> Result<CodexInstalledFileState, CodexProblem> {
        if self.identity != before.identity {
            return Err(CodexProblem::new("configuration-write-failed", None));
        }
        let source = String::from_utf8(self.bytes.clone())
            .map_err(|_| CodexProblem::new("configuration-write-failed", None))?;
        let mut document = source
            .parse::<DocumentMut>()
            .map_err(|_| CodexProblem::new("configuration-write-failed", None))?;
        apply_restore_union_document(&mut document, before, desired, provider_restore)
            .map_err(|_| CodexProblem::new("configuration-write-failed", None))?;
        Ok(planned_installed_file_state(
            &document,
            &historical_before.identity,
        ))
    }
}

impl ConfigSnapshot {
    pub(crate) fn identity(&self) -> &FileIdentity {
        &self.identity
    }

    #[allow(dead_code)]
    pub(crate) fn owned_fingerprint(&self) -> String {
        owned_semantic_fingerprint(&self.owned)
    }

    #[allow(dead_code)]
    pub(crate) fn unrelated_fingerprint(&self) -> String {
        semantic_fingerprint(&self.unrelated)
    }

    #[allow(dead_code)]
    pub(crate) fn as_desired_like(&self, committed: &DesiredCodexState) -> DesiredCodexState {
        DesiredCodexState {
            owned: self.owned.clone(),
            mode: committed.mode,
        }
    }

    pub(crate) fn as_adopted_direct(&self) -> DesiredCodexState {
        DesiredCodexState {
            owned: self.owned.clone(),
            mode: Some(ManagedCodexMode::Direct),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn provider_matches(&self, desired: &DesiredCodexState) -> bool {
        provider_semantically_matches(&self.owned, &desired.owned)
    }

    #[allow(dead_code)]
    pub(crate) fn credential_matches(&self, desired: &DesiredCodexState) -> bool {
        item_semantically_matches(
            &self.owned.provider_http_headers,
            &desired.owned.provider_http_headers,
        )
    }

    pub(crate) fn with_provider_restore_for(
        &self,
        desired: &DesiredCodexState,
    ) -> Result<Self, CodexProblem> {
        let key = desired.owned.effective_provider_key();
        if key.is_empty() {
            return Err(CodexProblem::new("invalid-configuration", None));
        }
        let provider_restore = match self
            .provider_restore
            .as_ref()
            .filter(|state| state.key() == key)
            .cloned()
        {
            Some(state) => state,
            None => provider_restore_from_unrelated(&self.unrelated, key),
        };
        if provider_restore.is_unrepresentable() {
            return Err(CodexProblem::new("invalid-configuration", None));
        }
        let mut before = self.clone();
        before.provider_restore = Some(provider_restore);
        Ok(before)
    }

    pub(crate) fn provider_restore(&self) -> Option<&CodexProviderRestoreState> {
        self.provider_restore.as_ref()
    }
}

pub struct CodexConfigCodec {
    file: ManagedFile,
}

impl CodexConfigCodec {
    pub fn for_user_home(user_home: &Path) -> Result<Self, CodexProblem> {
        Self::build(user_home, None, None)
    }

    pub fn for_user_home_with_pre_rename_hook(
        user_home: &Path,
        hook: PreRenameHook,
    ) -> Result<Self, CodexProblem> {
        Self::build(user_home, Some(hook), None)
    }

    pub fn for_user_home_with_exchange_hook(
        user_home: &Path,
        hook: ExchangeHook,
    ) -> Result<Self, CodexProblem> {
        Self::build(user_home, None, Some(hook))
    }

    fn build(
        user_home: &Path,
        hook: Option<PreRenameHook>,
        exchange_hook: Option<ExchangeHook>,
    ) -> Result<Self, CodexProblem> {
        let file = match (hook, exchange_hook) {
            (Some(hook), None) => {
                ManagedFile::with_pre_rename_hook(user_home, ".codex", "config.toml", hook)
            }
            (None, Some(hook)) => {
                ManagedFile::with_exchange_hook(user_home, ".codex", "config.toml", hook)
            }
            (None, None) => ManagedFile::in_configuration_home(user_home, ".codex", "config.toml"),
            (Some(_), Some(_)) => unreachable!("Codex test hooks are mutually exclusive"),
        };
        Ok(Self {
            file: file.map_err(|error| map_file_error(error, None))?,
        })
    }

    pub fn config_path(&self) -> &Path {
        self.file.path()
    }

    pub fn inspect(&self) -> Result<ConfigSnapshot, CodexProblem> {
        self.inspect_state(None)
            .map(ManagedCodexState::into_snapshot)
    }

    pub(crate) fn provider_for_import(
        &self,
    ) -> Result<(String, String, String, String, SecretString), CodexProblem> {
        let contents = self
            .file
            .read()
            .map_err(|error| map_file_error(error, Some(self.config_path())))?;
        let source = String::from_utf8(contents.bytes)
            .map_err(|_| CodexProblem::new("invalid-configuration", Some(self.config_path())))?;
        let document = source
            .parse::<DocumentMut>()
            .map_err(|_| CodexProblem::new("invalid-configuration", Some(self.config_path())))?;
        let provider_key = selected_provider_key(&document)
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| CodexProblem::new("invalid-configuration", Some(self.config_path())))?;
        let snapshot = snapshot_from_document(contents.identity, &document, &provider_key)?;
        let (name, model, base_url, credential) = snapshot
            .as_adopted_direct()
            .reconciliation_provider()
            .ok_or_else(|| CodexProblem::new("invalid-configuration", Some(self.config_path())))?;
        Ok((provider_key, name, model, base_url, credential))
    }

    #[allow(dead_code)]
    pub(crate) fn reconciliation_snapshot(&self) -> Result<(ConfigSnapshot, bool), CodexProblem> {
        let (snapshot, document) = self.read_snapshot()?;
        let profile_shadow = document
            .get("profile")
            .is_some_and(|profile| !profile.is_none());
        Ok((snapshot, profile_shadow))
    }

    pub(crate) fn reconciliation_snapshots_for(
        &self,
        committed: &DesiredCodexState,
    ) -> Result<(ConfigSnapshot, ConfigSnapshot, CodexObservedDocument, bool), CodexProblem> {
        let contents = self
            .file
            .read()
            .map_err(|error| map_file_error(error, Some(self.config_path())))?;
        let source = String::from_utf8(contents.bytes.clone()).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(self.config_path()))
        })?;
        let document = source.parse::<DocumentMut>().map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(self.config_path()))
        })?;
        let selected_provider_key = selected_provider_key(&document).unwrap_or_default();
        let managed = snapshot_from_document(
            contents.identity.clone(),
            &document,
            committed.owned.effective_provider_key(),
        )?;
        let selected = snapshot_from_document(
            contents.identity.clone(),
            &document,
            selected_provider_key.as_str(),
        )?;
        let observed_document = CodexObservedDocument {
            identity: contents.identity,
            bytes: contents.bytes,
        };
        let profile_shadow = document
            .get("profile")
            .is_some_and(|profile| !profile.is_none());
        Ok((managed, selected, observed_document, profile_shadow))
    }

    fn inspect_state(
        &self,
        committed: Option<(ManagedCodexMode, &DesiredCodexState)>,
    ) -> Result<ManagedCodexState, CodexProblem> {
        let (snapshot, document) = match committed {
            Some((_, expected)) => {
                self.read_snapshot_for_key(expected.owned.effective_provider_key())?
            }
            None => self.read_snapshot()?,
        };
        match committed {
            None => {
                if document
                    .get("model_providers")
                    .and_then(|providers| providers.get("muxvia_codex"))
                    .is_some()
                {
                    return Err(CodexProblem::new(
                        "configuration-collision",
                        Some(self.config_path()),
                    ));
                }
                Ok(ManagedCodexState::Unmanaged { snapshot })
            }
            Some((mode, expected)) => {
                if !owned_semantically_matches(&snapshot.owned, &expected.owned) {
                    return Err(CodexProblem::new(
                        "configuration-collision",
                        Some(self.config_path()),
                    ));
                }
                Ok(match mode {
                    ManagedCodexMode::Direct => ManagedCodexState::Direct { snapshot },
                    ManagedCodexMode::Takeover => ManagedCodexState::Takeover { snapshot },
                })
            }
        }
    }

    pub fn inspect_managed(
        &self,
        expected: &DesiredCodexState,
    ) -> Result<ConfigSnapshot, CodexProblem> {
        self.inspect_managed_state(expected)
            .map(ManagedCodexState::into_snapshot)
    }

    /// `expected` must be rebuilt from the authoritative committed Routing Service state,
    /// never inferred from the observed reserved provider table.
    pub(crate) fn inspect_managed_state(
        &self,
        expected: &DesiredCodexState,
    ) -> Result<ManagedCodexState, CodexProblem> {
        let mode = expected.mode.ok_or_else(|| {
            CodexProblem::new("configuration-collision", Some(self.config_path()))
        })?;
        self.inspect_state(Some((mode, expected)))
    }

    pub fn desired_takeover(
        &self,
        model: &str,
        base_url: &str,
        routing_credential: &str,
    ) -> DesiredCodexState {
        let header = serde_json::to_string(routing_credential)
            .expect("serializing a routing credential string cannot fail");
        DesiredCodexState {
            owned: OwnedCodexState {
                owned_provider_key: default_owned_provider_key(),
                model: desired_item(value(model)),
                model_provider: desired_item(value("muxvia_codex")),
                provider_name: desired_item(value("Muxvia")),
                provider_base_url: desired_item(value(base_url)),
                provider_wire_api: desired_item(value("responses")),
                provider_http_headers: parse_owned_item(&format!(
                    " {{ \"X-Muxvia-Routing-Credential\" = {header} }}"
                )),
                provider_supports_websockets: desired_item(value(false)),
            },
            mode: Some(ManagedCodexMode::Takeover),
        }
    }

    pub(crate) fn desired_takeover_with_ownership(
        &self,
        model: &str,
        base_url: &str,
        routing_credential: &str,
        ownership: &DesiredCodexState,
    ) -> DesiredCodexState {
        self.desired_takeover(model, base_url, routing_credential)
            .with_provider_ownership(ownership)
    }

    pub fn desired_direct(
        &self,
        model: &str,
        base_url: &str,
        provider_credential: &str,
    ) -> DesiredCodexState {
        let authorization = serde_json::to_string(&format!("Bearer {provider_credential}"))
            .expect("serializing a Provider credential string cannot fail");
        DesiredCodexState {
            owned: OwnedCodexState {
                owned_provider_key: default_owned_provider_key(),
                model: desired_item(value(model)),
                model_provider: desired_item(value("muxvia_codex")),
                provider_name: desired_item(value("Muxvia Direct")),
                provider_base_url: desired_item(value(base_url)),
                provider_wire_api: desired_item(value("responses")),
                provider_http_headers: parse_owned_item(&format!(
                    " {{ Authorization = {authorization} }}"
                )),
                provider_supports_websockets: desired_item(value(false)),
            },
            mode: Some(ManagedCodexMode::Direct),
        }
    }

    pub(crate) fn desired_direct_with_ownership(
        &self,
        model: &str,
        base_url: &str,
        provider_credential: &str,
        ownership: &DesiredCodexState,
    ) -> DesiredCodexState {
        self.desired_direct(model, base_url, provider_credential)
            .with_provider_ownership(ownership)
    }

    pub fn atomic_apply(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        self.write_owned(before, &desired.owned, false, true)?;
        self.verify(before, desired)
    }

    pub(crate) fn atomic_restore_union(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
        provider_restore: &CodexProviderRestoreState,
        historical_before: &ConfigSnapshot,
        installed_file_state: &CodexInstalledFileState,
    ) -> Result<(), CodexProblem> {
        let (current, mut document) =
            self.read_snapshot_for_key(before.owned.effective_provider_key())?;
        if current != *before {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(self.config_path()),
            ));
        }
        apply_restore_union_document(&mut document, before, desired, provider_restore).map_err(
            |_| CodexProblem::new("configuration-write-failed", Some(self.config_path())),
        )?;
        self.file
            .replace_with_mode_from(
                &before.identity,
                document.to_string().as_bytes(),
                !installed_file_state.exists,
                &historical_before.identity,
            )
            .map_err(|error| map_file_error(error, Some(self.config_path())))?;
        self.verify_restore_union(
            before,
            desired,
            provider_restore,
            Some(installed_file_state),
        )
    }

    pub(crate) fn verify_restore_union(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
        provider_restore: &CodexProviderRestoreState,
        installed_file_state: Option<&CodexInstalledFileState>,
    ) -> Result<(), CodexProblem> {
        let (current, document) =
            self.read_snapshot_for_key(desired.owned.effective_provider_key())?;
        if !restore_union_matches(
            &current,
            &document,
            before,
            desired,
            provider_restore,
            installed_file_state,
        ) {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(self.config_path()),
            ));
        }
        Ok(())
    }

    pub(crate) fn exact_rollback_restore_union(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
        provider_restore: &CodexProviderRestoreState,
        installed_file_state: Option<&CodexInstalledFileState>,
        original: &CodexObservedDocument,
    ) -> Result<(), CodexProblem> {
        let contents = self
            .file
            .read()
            .map_err(|error| map_file_error(error, Some(self.config_path())))?;
        if contents.identity.exists() == original.identity.exists()
            && contents.identity.mode() == original.identity.mode()
            && contents.bytes == original.bytes
        {
            return Ok(());
        }
        let source = String::from_utf8(contents.bytes)
            .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())))?;
        let document = source
            .parse::<DocumentMut>()
            .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())))?;
        let current = snapshot_from_document(
            contents.identity.clone(),
            &document,
            desired.owned.effective_provider_key(),
        )?;
        if !restore_union_matches(
            &current,
            &document,
            before,
            desired,
            provider_restore,
            installed_file_state,
        ) {
            return Err(CodexProblem::new(
                "recovery-required",
                Some(self.config_path()),
            ));
        }
        self.file
            .replace_with_mode_from(
                &contents.identity,
                &original.bytes,
                !original.identity.exists(),
                &original.identity,
            )
            .map_err(|error| map_file_error(error, Some(self.config_path())))?;
        let restored = self
            .file
            .read()
            .map_err(|error| map_file_error(error, Some(self.config_path())))?;
        if restored.identity.exists() != original.identity.exists()
            || restored.identity.mode() != original.identity.mode()
            || restored.bytes != original.bytes
        {
            return Err(CodexProblem::new(
                "recovery-required",
                Some(self.config_path()),
            ));
        }
        Ok(())
    }

    pub fn restore(
        &self,
        before: &ConfigSnapshot,
        expected_current: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        let current = self
            .read_snapshot_for_key(expected_current.owned.effective_provider_key())?
            .0;
        if !owned_semantically_matches(&current.owned, &expected_current.owned) {
            return Err(CodexProblem::new(
                "recovery-required",
                Some(self.config_path()),
            ));
        }
        let remove_file = !before.identity.exists()
            && current
                .unrelated
                .as_object()
                .is_some_and(serde_json::Map::is_empty);
        self.write_owned(&current, &before.owned, remove_file, false)?;
        let restored = self
            .read_snapshot_for_key(before.owned.effective_provider_key())?
            .0;
        if restored.owned != before.owned || restored.unrelated != current.unrelated {
            return Err(CodexProblem::new(
                "recovery-required",
                Some(self.config_path()),
            ));
        }
        Ok(())
    }

    pub fn restore_or_confirm_before(
        &self,
        before: &ConfigSnapshot,
        expected_current: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        if self.matches_before(before) {
            return Ok(());
        }
        self.restore(before, expected_current)?;
        if self.matches_before(before) {
            Ok(())
        } else {
            Err(CodexProblem::new(
                "recovery-required",
                Some(self.config_path()),
            ))
        }
    }

    pub(crate) fn restore_union_or_confirm_before(
        &self,
        before: &ConfigSnapshot,
        expected_current: &DesiredCodexState,
        provider_restore: &CodexProviderRestoreState,
        installed_file_state: Option<&CodexInstalledFileState>,
    ) -> Result<(), CodexProblem> {
        self.restore_union_or_confirm_before_inner(
            before,
            expected_current,
            provider_restore,
            installed_file_state,
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn restore_union_or_confirm_before_with_validation_hook(
        &self,
        before: &ConfigSnapshot,
        expected_current: &DesiredCodexState,
        provider_restore: &CodexProviderRestoreState,
        installed_file_state: Option<&CodexInstalledFileState>,
        validation_hook: &dyn Fn(),
    ) -> Result<(), CodexProblem> {
        self.restore_union_or_confirm_before_inner(
            before,
            expected_current,
            provider_restore,
            installed_file_state,
            Some(validation_hook),
        )
    }

    fn restore_union_or_confirm_before_inner(
        &self,
        before: &ConfigSnapshot,
        expected_current: &DesiredCodexState,
        provider_restore: &CodexProviderRestoreState,
        installed_file_state: Option<&CodexInstalledFileState>,
        validation_hook: Option<&dyn Fn()>,
    ) -> Result<(), CodexProblem> {
        let (current, mut document) =
            self.read_snapshot_for_key(expected_current.owned.effective_provider_key())?;
        if snapshot_matches_before(&current, before) {
            return Ok(());
        }
        if !restore_union_matches(
            &current,
            &document,
            before,
            expected_current,
            provider_restore,
            installed_file_state,
        ) {
            return Err(CodexProblem::new(
                "recovery-required",
                Some(self.config_path()),
            ));
        }
        if let Some(hook) = validation_hook {
            hook();
        }
        apply_owned(
            &mut document,
            expected_current.owned.effective_provider_key(),
            &before.owned,
            false,
        )
        .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())))?;
        if expected_current.owned.effective_provider_key() != before.owned.effective_provider_key()
        {
            let prior_expected_provider = provider_restore_from_unrelated_unchecked(
                &before.unrelated,
                expected_current.owned.effective_provider_key(),
            )?;
            apply_provider_restore(&mut document, &prior_expected_provider)
                .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())))?;
        }
        let remove_file = !before.identity.exists() && document.as_table().is_empty();
        self.file
            .replace_with_mode_from(
                &current.identity,
                document.to_string().as_bytes(),
                remove_file,
                &before.identity,
            )
            .map_err(|error| map_file_error(error, Some(self.config_path())))?;
        if self.matches_before(before) {
            Ok(())
        } else {
            Err(CodexProblem::new(
                "recovery-required",
                Some(self.config_path()),
            ))
        }
    }

    pub fn verify(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        let current = self
            .read_snapshot_for_key(desired.owned.effective_provider_key())?
            .0;
        if !owned_semantically_matches(&current.owned, &desired.owned)
            || current.unrelated != before.unrelated
        {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(self.config_path()),
            ));
        }
        Ok(())
    }

    pub async fn reconcile_pending(&self, store: &StateStore) -> Result<(), CodexProblem> {
        for intent in store
            .pending_recovery_intents()
            .await
            .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())))?
        {
            if intent.target() != crate::control::protocol::Target::Codex {
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
    ) -> Result<(), CodexProblem> {
        if intent.config_path() != self.config_path() {
            store
                .set_recovery_state(intent.id(), RecoveryState::RecoveryRequired)
                .await
                .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())))?;
            return Err(CodexProblem::new(
                "recovery-required",
                Some(self.config_path()),
            ));
        }
        let before_current =
            match self.read_snapshot_for_key(intent.before().owned.effective_provider_key()) {
                Ok((snapshot, _)) => snapshot,
                Err(_) => {
                    self.mark_recovery_required(store, intent).await?;
                    return Err(CodexProblem::new(
                        "recovery-required",
                        Some(self.config_path()),
                    ));
                }
            };
        if before_current.owned == intent.before().owned
            && before_current.unrelated == intent.before().unrelated
        {
            return store
                .set_recovery_state(intent.id(), RecoveryState::RolledBack)
                .await
                .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())));
        }
        let desired_current =
            match self.read_snapshot_for_key(intent.desired().owned.effective_provider_key()) {
                Ok((snapshot, _)) => snapshot,
                Err(_) => {
                    self.mark_recovery_required(store, intent).await?;
                    return Err(CodexProblem::new(
                        "recovery-required",
                        Some(self.config_path()),
                    ));
                }
            };
        if owned_semantically_matches(&desired_current.owned, &intent.desired().owned)
            && desired_current.unrelated == intent.before().unrelated
            && self.restore(intent.before(), intent.desired()).is_ok()
            && self.matches_before(intent.before())
        {
            return store
                .set_recovery_state(intent.id(), RecoveryState::RolledBack)
                .await
                .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())));
        }
        self.mark_recovery_required(store, intent).await?;
        Err(CodexProblem::new(
            "recovery-required",
            Some(self.config_path()),
        ))
    }

    async fn mark_recovery_required(
        &self,
        store: &StateStore,
        intent: &RecoveryIntent,
    ) -> Result<(), CodexProblem> {
        store
            .set_recovery_state(intent.id(), RecoveryState::RecoveryRequired)
            .await
            .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())))
    }

    fn matches_before(&self, before: &ConfigSnapshot) -> bool {
        match self.read_snapshot_for_key(before.owned.effective_provider_key()) {
            Ok((current, _)) => snapshot_matches_before(&current, before),
            Err(_) => false,
        }
    }

    fn write_owned(
        &self,
        expected: &ConfigSnapshot,
        owned: &OwnedCodexState,
        remove_file: bool,
        preserve_decor: bool,
    ) -> Result<(), CodexProblem> {
        let (current, mut document) =
            self.read_snapshot_for_key(expected.owned.effective_provider_key())?;
        if current != *expected {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(self.config_path()),
            ));
        }
        apply_owned(
            &mut document,
            expected.owned.effective_provider_key(),
            owned,
            preserve_decor,
        )
        .map_err(|_| CodexProblem::new("configuration-write-failed", Some(self.config_path())))?;
        self.file
            .replace(
                &expected.identity,
                document.to_string().as_bytes(),
                remove_file,
            )
            .map_err(|error| map_file_error(error, Some(self.config_path())))
    }

    fn read_snapshot(&self) -> Result<(ConfigSnapshot, DocumentMut), CodexProblem> {
        self.read_snapshot_for_key("muxvia_codex")
    }

    fn read_snapshot_for_key(
        &self,
        provider_key: &str,
    ) -> Result<(ConfigSnapshot, DocumentMut), CodexProblem> {
        let contents = self
            .file
            .read()
            .map_err(|error| map_file_error(error, Some(self.config_path())))?;
        let source = String::from_utf8(contents.bytes).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(self.config_path()))
        })?;
        let document = source.parse::<DocumentMut>().map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(self.config_path()))
        })?;
        let snapshot = snapshot_from_document(contents.identity, &document, provider_key)?;
        Ok((snapshot, document))
    }
}

fn snapshot_from_document(
    identity: FileIdentity,
    document: &DocumentMut,
    provider_key: &str,
) -> Result<ConfigSnapshot, CodexProblem> {
    Ok(ConfigSnapshot {
        identity,
        owned: capture_owned_for_key(document, provider_key)?,
        unrelated: unrelated_projection_for_key(document, provider_key)?,
        provider_restore: capture_selected_provider_restore(document, provider_key),
    })
}

fn map_file_error(error: ManagedFileError, path: Option<&Path>) -> CodexProblem {
    let code = match error {
        ManagedFileError::WriteFailed => "configuration-write-failed",
        ManagedFileError::RecoveryRequired => "recovery-required",
    };
    CodexProblem::new(code, path)
}

fn selected_provider_key(document: &DocumentMut) -> Option<String> {
    document
        .get("model_provider")
        .and_then(Item::as_value)
        .and_then(value_semantic)
        .and_then(|value| value.as_str().map(str::to_owned))
}

fn capture_selected_provider_restore(
    document: &DocumentMut,
    owned_provider_key: &str,
) -> Option<CodexProviderRestoreState> {
    let key = selected_provider_key(document)?;
    if key.trim().is_empty() || key == owned_provider_key {
        return None;
    }
    Some(capture_provider_restore_for_key(document, &key))
}

fn capture_provider_restore_for_key(
    document: &DocumentMut,
    key: &str,
) -> CodexProviderRestoreState {
    let Some(provider) = document
        .get("model_providers")
        .and_then(|providers| providers.get(key))
        .and_then(Item::as_table_like)
    else {
        return CodexProviderRestoreState::Absent {
            key: key.to_owned(),
        };
    };
    let name = nested_item_text(document, key, "name");
    let base_url = nested_item_text(document, key, "base_url");
    let wire_api = nested_item_text(document, key, "wire_api");
    let http_headers = nested_item_text(document, key, "http_headers");
    let supports_websockets = nested_item_text(document, key, "supports_websockets");
    if [
        &name,
        &base_url,
        &wire_api,
        &http_headers,
        &supports_websockets,
    ]
    .into_iter()
    .all(Option::is_none)
    {
        return CodexProviderRestoreState::Absent {
            key: key.to_owned(),
        };
    }
    let strings_are_representable = [&name, &base_url].into_iter().all(|item| {
        item.as_ref().is_none_or(|item| {
            item.semantic
                .as_str()
                .is_some_and(|value| !value.trim().is_empty())
        })
    });
    let wire_is_representable = wire_api.as_ref().is_none_or(|item| {
        item.semantic
            .as_str()
            .is_some_and(|value| value == "responses")
    });
    let websocket_is_representable = supports_websockets
        .as_ref()
        .is_none_or(|item| item.semantic.as_bool() == Some(false));
    let auth_is_representable = http_headers.as_ref().is_none_or(|item| {
        item.semantic.as_object().is_some_and(|headers| {
            headers.len() == 1
                && headers
                    .get("Authorization")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .is_some_and(|credential| !credential.trim().is_empty())
        })
    });
    if !strings_are_representable
        || !wire_is_representable
        || !websocket_is_representable
        || !auth_is_representable
        || provider.get("name").is_some() != name.is_some()
        || provider.get("base_url").is_some() != base_url.is_some()
        || provider.get("wire_api").is_some() != wire_api.is_some()
        || provider.get("http_headers").is_some() != http_headers.is_some()
        || provider.get("supports_websockets").is_some() != supports_websockets.is_some()
    {
        return CodexProviderRestoreState::Unrepresentable {
            key: key.to_owned(),
        };
    }
    CodexProviderRestoreState::Present {
        key: key.to_owned(),
        fields: Box::new(CodexProviderRestoreFields {
            name,
            base_url,
            wire_api,
            http_headers,
            supports_websockets,
        }),
    }
}

fn provider_restore_from_unrelated(
    unrelated: &serde_json::Value,
    key: &str,
) -> CodexProviderRestoreState {
    let Some(provider) = unrelated
        .get("model_providers")
        .and_then(|providers| providers.get(key))
        .and_then(serde_json::Value::as_object)
    else {
        return CodexProviderRestoreState::Absent {
            key: key.to_owned(),
        };
    };
    let string_item = |field: &str| -> Result<Option<OwnedItem>, ()> {
        match provider.get(field) {
            None => Ok(None),
            Some(json_value) => {
                let string = json_value
                    .as_str()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or(())?;
                Ok(desired_item(value(string)))
            }
        }
    };
    let name = string_item("name");
    let base_url = string_item("base_url");
    let wire_api = match provider.get("wire_api") {
        None => Ok(None),
        Some(json_value) if json_value.as_str() == Some("responses") => {
            Ok(desired_item(value("responses")))
        }
        Some(_) => Err(()),
    };
    let supports_websockets = match provider.get("supports_websockets") {
        None => Ok(None),
        Some(json_value) if json_value.as_bool() == Some(false) => Ok(desired_item(value(false))),
        Some(_) => Err(()),
    };
    let http_headers = match provider.get("http_headers") {
        None => Ok(None),
        Some(value) => value
            .as_object()
            .filter(|headers| headers.len() == 1)
            .and_then(|headers| headers.get("Authorization"))
            .and_then(serde_json::Value::as_str)
            .and_then(|authorization| {
                authorization
                    .strip_prefix("Bearer ")
                    .filter(|credential| !credential.trim().is_empty())
                    .map(|_| authorization)
            })
            .and_then(|authorization| {
                let encoded = serde_json::to_string(authorization).ok()?;
                parse_owned_item(&format!(" {{ Authorization = {encoded} }}"))
            })
            .map(Some)
            .ok_or(()),
    };
    if [
        name.as_ref().ok(),
        base_url.as_ref().ok(),
        wire_api.as_ref().ok(),
        http_headers.as_ref().ok(),
        supports_websockets.as_ref().ok(),
    ]
    .into_iter()
    .all(|item| item.is_some_and(Option::is_none))
    {
        return CodexProviderRestoreState::Absent {
            key: key.to_owned(),
        };
    }
    match (name, base_url, wire_api, http_headers, supports_websockets) {
        (Ok(name), Ok(base_url), Ok(wire_api), Ok(http_headers), Ok(supports_websockets)) => {
            CodexProviderRestoreState::Present {
                key: key.to_owned(),
                fields: Box::new(CodexProviderRestoreFields {
                    name,
                    base_url,
                    wire_api,
                    http_headers,
                    supports_websockets,
                }),
            }
        }
        _ => CodexProviderRestoreState::Unrepresentable {
            key: key.to_owned(),
        },
    }
}

fn provider_restore_from_unrelated_unchecked(
    unrelated: &serde_json::Value,
    key: &str,
) -> Result<CodexProviderRestoreState, CodexProblem> {
    let Some(provider) = unrelated
        .get("model_providers")
        .and_then(|providers| providers.get(key))
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(CodexProviderRestoreState::Absent {
            key: key.to_owned(),
        });
    };
    let item = |field: &str| -> Result<Option<OwnedItem>, CodexProblem> {
        provider
            .get(field)
            .map(|semantic| {
                let rendered = semantic
                    .serialize(toml_edit::ser::ValueSerializer::new())
                    .map(|value| value_to_owned_rendered(&value))
                    .map_err(|_| CodexProblem::new("recovery-required", None))?;
                Ok(OwnedItem {
                    rendered,
                    semantic: semantic.clone(),
                })
            })
            .transpose()
    };
    Ok(CodexProviderRestoreState::Present {
        key: key.to_owned(),
        fields: Box::new(CodexProviderRestoreFields {
            name: item("name")?,
            base_url: item("base_url")?,
            wire_api: item("wire_api")?,
            http_headers: item("http_headers")?,
            supports_websockets: item("supports_websockets")?,
        }),
    })
}

fn capture_owned_for_key(
    document: &DocumentMut,
    provider_key: &str,
) -> Result<OwnedCodexState, CodexProblem> {
    for key in ["model", "model_provider"] {
        if document.get(key).is_some_and(|item| !item.is_value()) {
            return Err(CodexProblem::new("configuration-collision", None));
        }
    }
    Ok(OwnedCodexState {
        owned_provider_key: provider_key.to_owned(),
        model: item_text(document.get("model")),
        model_provider: item_text(document.get("model_provider")),
        provider_name: nested_item_text(document, provider_key, "name"),
        provider_base_url: nested_item_text(document, provider_key, "base_url"),
        provider_wire_api: nested_item_text(document, provider_key, "wire_api"),
        provider_http_headers: nested_item_text(document, provider_key, "http_headers"),
        provider_supports_websockets: nested_item_text(
            document,
            provider_key,
            "supports_websockets",
        ),
    })
}

fn item_text(item: Option<&Item>) -> Option<OwnedItem> {
    item.filter(|item| !item.is_none())
        .and_then(|item| item.as_value())
        .and_then(|value| {
            Some(OwnedItem {
                rendered: value.to_string(),
                semantic: value_semantic(value)?,
            })
        })
}

fn nested_item_text(document: &DocumentMut, provider_key: &str, key: &str) -> Option<OwnedItem> {
    document
        .get("model_providers")?
        .get(provider_key)?
        .get(key)
        .filter(|item| !item.is_none())
        .and_then(Item::as_value)
        .and_then(|value| {
            Some(OwnedItem {
                rendered: value.to_string(),
                semantic: value_semantic(value)?,
            })
        })
}

fn value_semantic(value: &toml_edit::Value) -> Option<serde_json::Value> {
    let document = format!("owned = {value}\n").parse::<DocumentMut>().ok()?;
    let mut tree: serde_json::Value = toml_edit::de::from_document(document).ok()?;
    tree.as_object_mut()?.remove("owned")
}

fn desired_item(item: Item) -> Option<OwnedItem> {
    item.as_value().and_then(|value| {
        Some(OwnedItem {
            rendered: value_to_owned_rendered(value),
            semantic: value_semantic(value)?,
        })
    })
}

fn parse_owned_item(rendered: &str) -> Option<OwnedItem> {
    let document = format!("owned ={rendered}\n").parse::<DocumentMut>().ok()?;
    item_text(document.get("owned"))
}

fn value_to_owned_rendered(value: &toml_edit::Value) -> String {
    let mut value = value.clone();
    value.decor_mut().clear();
    format!(" {value}")
}

fn owned_semantically_matches(left: &OwnedCodexState, right: &OwnedCodexState) -> bool {
    left.effective_provider_key() == right.effective_provider_key()
        && item_semantically_matches(&left.model, &right.model)
        && item_semantically_matches(&left.model_provider, &right.model_provider)
        && item_semantically_matches(&left.provider_name, &right.provider_name)
        && item_semantically_matches(&left.provider_base_url, &right.provider_base_url)
        && item_semantically_matches(&left.provider_wire_api, &right.provider_wire_api)
        && item_semantically_matches(&left.provider_http_headers, &right.provider_http_headers)
        && item_semantically_matches(
            &left.provider_supports_websockets,
            &right.provider_supports_websockets,
        )
}

#[allow(dead_code)]
fn provider_semantically_matches(left: &OwnedCodexState, right: &OwnedCodexState) -> bool {
    left.effective_provider_key() == right.effective_provider_key()
        && item_semantically_matches(&left.model, &right.model)
        && item_semantically_matches(&left.model_provider, &right.model_provider)
        && item_semantically_matches(&left.provider_name, &right.provider_name)
        && item_semantically_matches(&left.provider_base_url, &right.provider_base_url)
        && item_semantically_matches(&left.provider_wire_api, &right.provider_wire_api)
        && item_semantically_matches(
            &left.provider_supports_websockets,
            &right.provider_supports_websockets,
        )
}

fn item_semantically_matches(left: &Option<OwnedItem>, right: &Option<OwnedItem>) -> bool {
    left.as_ref().map(|item| &item.semantic) == right.as_ref().map(|item| &item.semantic)
}

fn snapshot_file_state_matches(left: &FileIdentity, right: &FileIdentity) -> bool {
    left.exists() == right.exists() && (!left.exists() || left.mode() == right.mode())
}

fn snapshot_matches_before(current: &ConfigSnapshot, before: &ConfigSnapshot) -> bool {
    current.owned == before.owned
        && current.unrelated == before.unrelated
        && snapshot_file_state_matches(&current.identity, &before.identity)
}

fn installed_file_state_matches(
    identity: &FileIdentity,
    expected: &CodexInstalledFileState,
) -> bool {
    identity.exists() == expected.exists && (!expected.exists || identity.mode() == expected.mode)
}

#[allow(dead_code)]
fn semantic_fingerprint(value: &impl Serialize) -> String {
    let bytes =
        serde_json::to_vec(value).expect("serializing captured Codex semantics cannot fail");
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let mut encoded = String::with_capacity(digest.as_ref().len() * 2);
    for byte in digest.as_ref() {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[allow(dead_code)]
fn owned_semantic_fingerprint(owned: &OwnedCodexState) -> String {
    fn semantic(item: &Option<OwnedItem>) -> Option<&serde_json::Value> {
        item.as_ref().map(|item| &item.semantic)
    }
    semantic_fingerprint(&(
        owned.effective_provider_key(),
        semantic(&owned.model),
        semantic(&owned.model_provider),
        semantic(&owned.provider_name),
        semantic(&owned.provider_base_url),
        semantic(&owned.provider_wire_api),
        semantic(&owned.provider_http_headers),
        semantic(&owned.provider_supports_websockets),
    ))
}

fn unrelated_projection_for_key(
    document: &DocumentMut,
    provider_key: &str,
) -> Result<serde_json::Value, CodexProblem> {
    let mut clone = document.clone();
    clone.remove("model");
    clone.remove("model_provider");
    if let Some(providers) = clone
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        if let Some(provider) = providers
            .get_mut(provider_key)
            .and_then(Item::as_table_like_mut)
        {
            for key in [
                "name",
                "base_url",
                "wire_api",
                "http_headers",
                "supports_websockets",
            ] {
                provider.remove(key);
            }
        }
        if providers
            .get(provider_key)
            .and_then(Item::as_table_like)
            .is_some_and(|provider| provider.is_empty())
        {
            providers.remove(provider_key);
        }
    }
    if clone
        .get("model_providers")
        .and_then(Item::as_table_like)
        .is_some_and(|providers| providers.is_empty())
    {
        clone.remove("model_providers");
    }
    toml_edit::de::from_document(clone)
        .map_err(|_| CodexProblem::new("configuration-write-failed", None))
}

fn unrelated_without_provider_fields(
    unrelated: &serde_json::Value,
    provider_key: &str,
) -> serde_json::Value {
    let mut unrelated = unrelated.clone();
    if let Some(providers) = unrelated
        .get_mut("model_providers")
        .and_then(serde_json::Value::as_object_mut)
    {
        if let Some(provider) = providers
            .get_mut(provider_key)
            .and_then(serde_json::Value::as_object_mut)
        {
            for key in [
                "name",
                "base_url",
                "wire_api",
                "http_headers",
                "supports_websockets",
            ] {
                provider.remove(key);
            }
            if provider.is_empty() {
                providers.remove(provider_key);
            }
        }
        if providers.is_empty() {
            unrelated
                .as_object_mut()
                .map(|root| root.remove("model_providers"));
        }
    }
    unrelated
}

fn restore_union_matches(
    current: &ConfigSnapshot,
    document: &DocumentMut,
    before: &ConfigSnapshot,
    desired: &DesiredCodexState,
    provider_restore: &CodexProviderRestoreState,
    installed_file_state: Option<&CodexInstalledFileState>,
) -> bool {
    let restored_provider = capture_provider_restore_for_key(document, provider_restore.key());
    let before_unrelated = unrelated_without_provider_fields(
        &before.unrelated,
        desired.owned.effective_provider_key(),
    );
    let current_unrelated =
        unrelated_without_provider_fields(&current.unrelated, provider_restore.key());
    let owned_matches = owned_semantically_matches(&current.owned, &desired.owned);
    let provider_matches = restored_provider == *provider_restore;
    let unrelated_matches = current_unrelated == before_unrelated;
    let file_state_matches = installed_file_state
        .is_none_or(|expected| installed_file_state_matches(&current.identity, expected));
    owned_matches && provider_matches && unrelated_matches && file_state_matches
}

fn apply_restore_union_document(
    document: &mut DocumentMut,
    before: &ConfigSnapshot,
    desired: &DesiredCodexState,
    provider_restore: &CodexProviderRestoreState,
) -> Result<(), ()> {
    apply_owned(
        document,
        before.owned.effective_provider_key(),
        &desired.owned,
        false,
    )?;
    apply_provider_restore(document, provider_restore)
}

fn planned_installed_file_state(
    document: &DocumentMut,
    historical_identity: &FileIdentity,
) -> CodexInstalledFileState {
    let remove_file = !historical_identity.exists() && document.as_table().is_empty();
    CodexInstalledFileState {
        exists: !remove_file,
        mode: (!remove_file).then_some(historical_identity.mode().unwrap_or(0o600)),
    }
}

fn apply_provider_restore(
    document: &mut DocumentMut,
    provider_restore: &CodexProviderRestoreState,
) -> Result<(), ()> {
    let (key, fields) = match provider_restore {
        CodexProviderRestoreState::Absent { key } => (key.as_str(), None),
        CodexProviderRestoreState::Present { key, fields } => (
            key.as_str(),
            Some([
                ("name", fields.name.as_ref()),
                ("base_url", fields.base_url.as_ref()),
                ("wire_api", fields.wire_api.as_ref()),
                ("http_headers", fields.http_headers.as_ref()),
                ("supports_websockets", fields.supports_websockets.as_ref()),
            ]),
        ),
        CodexProviderRestoreState::Unrepresentable { .. } => return Err(()),
    };
    if fields
        .as_ref()
        .is_some_and(|fields| fields.iter().any(|(_, value)| value.is_some()))
    {
        if document.get("model_providers").is_none() {
            document["model_providers"] = Item::Table(Table::new());
        }
        let providers = document["model_providers"].as_table_mut().ok_or(())?;
        if providers.get(key).is_none() {
            providers[key] = Item::Table(Table::new());
        }
        let provider = providers[key].as_table_mut().ok_or(())?;
        for (field, value) in fields.expect("checked provider fields") {
            if let Some(value) = value {
                set_table_item(provider, field, value, false)?;
            } else {
                provider.remove(field);
            }
        }
    } else if let Some(providers) = document
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        if let Some(provider) = providers.get_mut(key).and_then(Item::as_table_like_mut) {
            for field in [
                "name",
                "base_url",
                "wire_api",
                "http_headers",
                "supports_websockets",
            ] {
                provider.remove(field);
            }
            if provider.is_empty() {
                providers.remove(key);
            }
        }
        if providers.is_empty() {
            document.remove("model_providers");
        }
    }
    Ok(())
}

fn apply_owned(
    document: &mut DocumentMut,
    current_provider_key: &str,
    owned: &OwnedCodexState,
    preserve_decor: bool,
) -> Result<(), ()> {
    let provider_key = owned.effective_provider_key();
    if current_provider_key != provider_key
        && !current_provider_key.is_empty()
        && let Some(providers) = document
            .get_mut("model_providers")
            .and_then(Item::as_table_like_mut)
        && let Some(prior) = providers
            .get_mut(current_provider_key)
            .and_then(Item::as_table_like_mut)
    {
        for key in [
            "name",
            "base_url",
            "wire_api",
            "http_headers",
            "supports_websockets",
        ] {
            prior.remove(key);
        }
        if prior.is_empty() {
            providers.remove(current_provider_key);
        }
    }
    set_top_level(document, "model", owned.model.as_ref(), preserve_decor)?;
    set_top_level(
        document,
        "model_provider",
        owned.model_provider.as_ref(),
        preserve_decor,
    )?;
    let fields = [
        ("name", owned.provider_name.as_ref()),
        ("base_url", owned.provider_base_url.as_ref()),
        ("wire_api", owned.provider_wire_api.as_ref()),
        ("http_headers", owned.provider_http_headers.as_ref()),
        (
            "supports_websockets",
            owned.provider_supports_websockets.as_ref(),
        ),
    ];
    if fields.iter().any(|(_, value)| value.is_some()) && !provider_key.is_empty() {
        if document.get("model_providers").is_none() {
            document["model_providers"] = Item::Table(Table::new());
        }
        let providers = document["model_providers"].as_table_mut().ok_or(())?;
        if providers.get(provider_key).is_none() {
            providers[provider_key] = Item::Table(Table::new());
        }
        let provider = providers[provider_key].as_table_mut().ok_or(())?;
        for (key, encoded) in fields {
            if let Some(encoded) = encoded {
                set_table_item(provider, key, encoded, preserve_decor)?;
            } else {
                provider.remove(key);
            }
        }
    } else if !provider_key.is_empty()
        && let Some(providers) = document
            .get_mut("model_providers")
            .and_then(Item::as_table_like_mut)
        && let Some(provider) = providers
            .get_mut(provider_key)
            .and_then(Item::as_table_like_mut)
    {
        for key in [
            "name",
            "base_url",
            "wire_api",
            "http_headers",
            "supports_websockets",
        ] {
            provider.remove(key);
        }
        if provider.is_empty() {
            providers.remove(provider_key);
        }
    }
    if document
        .get("model_providers")
        .and_then(Item::as_table_like)
        .is_some_and(|providers| providers.is_empty())
    {
        document.remove("model_providers");
    }
    Ok(())
}

fn set_top_level(
    document: &mut DocumentMut,
    key: &str,
    encoded: Option<&OwnedItem>,
    preserve_decor: bool,
) -> Result<(), ()> {
    if let Some(encoded) = encoded {
        let decor = preserve_decor
            .then(|| value_decor(document.get(key)))
            .flatten();
        document[key] = parse_value(encoded)?;
        restore_decor(document.get_mut(key), decor);
    } else {
        document.remove(key);
    }
    Ok(())
}

fn set_table_item(
    table: &mut Table,
    key: &str,
    encoded: &OwnedItem,
    preserve_decor: bool,
) -> Result<(), ()> {
    let decor = preserve_decor
        .then(|| value_decor(table.get(key)))
        .flatten();
    table[key] = parse_value(encoded)?;
    restore_decor(table.get_mut(key), decor);
    Ok(())
}

fn parse_value(encoded: &OwnedItem) -> Result<Item, ()> {
    let document = format!("owned ={}\n", encoded.rendered)
        .parse::<DocumentMut>()
        .map_err(|_| ())?;
    document.get("owned").cloned().ok_or(())
}

type OwnedDecor = (Option<toml_edit::RawString>, Option<toml_edit::RawString>);

fn value_decor(item: Option<&Item>) -> Option<OwnedDecor> {
    let decor = item?.as_value()?.decor();
    Some((decor.prefix().cloned(), decor.suffix().cloned()))
}

fn restore_decor(item: Option<&mut Item>, decor: Option<OwnedDecor>) {
    if let (Some(value), Some((prefix, suffix))) = (item.and_then(Item::as_value_mut), decor) {
        value.decor_mut().clear();
        if let Some(prefix) = prefix {
            value.decor_mut().set_prefix(prefix);
        }
        if let Some(suffix) = suffix {
            value.decor_mut().set_suffix(suffix);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CodexConfigCodec, ManagedCodexMode, ManagedCodexState};
    use crate::{
        home::MuxviaHome,
        state::{RecoveryIntent, RecoveryState, StateStore},
    };
    use std::fs;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[test]
    fn managed_state_is_typed_only_by_the_callers_committed_expectation() {
        let root = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(root.path()).unwrap();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(codec.config_path(), "approval_policy = \"never\"\n").unwrap();

        assert!(matches!(
            codec.inspect_state(None).unwrap(),
            ManagedCodexState::Unmanaged { .. }
        ));

        let before = codec.inspect().unwrap();
        let direct = codec.desired_direct(
            "model-a",
            "https://provider.example/api/v1",
            "provider-secret",
        );
        codec.atomic_apply(&before, &direct).unwrap();
        assert!(matches!(
            codec
                .inspect_state(Some((ManagedCodexMode::Direct, &direct)))
                .unwrap(),
            ManagedCodexState::Direct { .. }
        ));
        assert_eq!(
            codec.inspect_state(None).unwrap_err().code(),
            "configuration-collision"
        );

        let direct_snapshot = codec.inspect_managed(&direct).unwrap();
        let takeover =
            codec.desired_takeover("model-b", "http://127.0.0.1:43123/v1", "route-secret");
        codec.restore(&before, &direct).unwrap();
        let unmanaged = codec.inspect().unwrap();
        codec.atomic_apply(&unmanaged, &takeover).unwrap();
        assert!(matches!(
            codec
                .inspect_state(Some((ManagedCodexMode::Takeover, &takeover)))
                .unwrap(),
            ManagedCodexState::Takeover { .. }
        ));

        assert!(!format!("{direct_snapshot:?}").contains("provider-secret"));
    }

    #[test]
    fn legacy_snapshot_defaults_to_redacted_bound_muxvia_provider_ownership() {
        let root = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(root.path()).unwrap();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(
            codec.config_path(),
            r#"model = "legacy-model"
model_provider = 7
[model_providers.muxvia_codex]
name = "Legacy bound provider"
base_url = "https://legacy.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer LEGACY_SECRET_97502" }
supports_websockets = false
"#,
        )
        .unwrap();
        let (snapshot, _) = codec.read_snapshot_for_key("muxvia_codex").unwrap();
        let mut legacy = serde_json::to_value(&snapshot).unwrap();
        legacy["owned"]
            .as_object_mut()
            .unwrap()
            .remove("owned_provider_key");

        let decoded: super::ConfigSnapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.owned.effective_provider_key(), "muxvia_codex");
        let diagnostic = format!("{:?}", decoded.owned);
        assert!(!diagnostic.contains("muxvia_codex"));
        assert!(!diagnostic.contains("LEGACY_SECRET_97502"));
    }

    #[tokio::test]
    async fn pending_cross_key_activation_before_write_recovers_as_rolled_back() {
        let root = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(root.path()).unwrap();
        let historical = codec.inspect().unwrap();
        let committed = codec.desired_direct(
            "committed-model",
            "https://committed.example/v1",
            "COMMITTED_SECRET_97205",
        );
        codec.atomic_apply(&historical, &committed).unwrap();
        let managed = fs::read_to_string(codec.config_path()).unwrap();
        fs::write(
            codec.config_path(),
            managed.replace(
                "model_provider = \"muxvia_codex\"",
                "model_provider = \"external\"",
            ) + r#"
[model_providers.external]
name = "External"
base_url = "https://external.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer EXTERNAL_SECRET_97206" }
supports_websockets = false
"#,
        )
        .unwrap();
        let (_, external, _, _) = codec.reconciliation_snapshots_for(&committed).unwrap();
        let external_desired = external.as_adopted_direct();
        let candidate = codec.desired_takeover_with_ownership(
            "candidate-model",
            "http://127.0.0.1:43123/v1",
            "candidate-route-secret",
            &external_desired,
        );
        let recovery = RecoveryIntent::pending(
            Uuid::new_v4(),
            Uuid::new_v4(),
            codec.config_path().to_owned(),
            external,
            candidate,
            0,
        );
        let store = StateStore::open(&MuxviaHome::from_user_home(root.path()))
            .await
            .unwrap();
        store.insert_recovery_intent(&recovery).await.unwrap();

        codec.reconcile_pending(&store).await.unwrap();

        assert_eq!(
            store
                .recovery_intent(recovery.id())
                .await
                .unwrap()
                .unwrap()
                .state(),
            RecoveryState::RolledBack
        );
    }
}
