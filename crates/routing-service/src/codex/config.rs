use std::{fmt, path::Path};

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, value};

use super::CodexProblem;
use crate::{
    config::managed_file::{ExchangeHook, ManagedFile, ManagedFileError, PreRenameHook},
    state::{RecoveryIntent, RecoveryState, StateStore},
};

pub use crate::config::managed_file::FileIdentity;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnedCodexState {
    model: Option<OwnedItem>,
    model_provider: Option<OwnedItem>,
    provider_name: Option<OwnedItem>,
    provider_base_url: Option<OwnedItem>,
    provider_wire_api: Option<OwnedItem>,
    provider_http_headers: Option<OwnedItem>,
    provider_supports_websockets: Option<OwnedItem>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct OwnedItem {
    rendered: String,
    semantic: serde_json::Value,
}

impl fmt::Debug for OwnedCodexState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OwnedCodexState")
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

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigSnapshot {
    identity: FileIdentity,
    owned: OwnedCodexState,
    unrelated: serde_json::Value,
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
            .finish()
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

    #[allow(dead_code)]
    pub(crate) fn reconciliation_snapshot(&self) -> Result<(ConfigSnapshot, bool), CodexProblem> {
        let (snapshot, document) = self.read_snapshot()?;
        let profile_shadow = document
            .get("profile")
            .is_some_and(|profile| !profile.is_none());
        Ok((snapshot, profile_shadow))
    }

    fn inspect_state(
        &self,
        committed: Option<(ManagedCodexMode, &DesiredCodexState)>,
    ) -> Result<ManagedCodexState, CodexProblem> {
        let (snapshot, document) = self.read_snapshot()?;
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

    pub fn atomic_apply(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        self.write_owned(before, &desired.owned, false, true)?;
        self.verify(before, desired)
    }

    pub fn restore(
        &self,
        before: &ConfigSnapshot,
        expected_current: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        let current = self.read_snapshot()?.0;
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
        let restored = self.read_snapshot()?.0;
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

    pub fn verify(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        let current = self.read_snapshot()?.0;
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
        let current = match self.read_snapshot() {
            Ok((snapshot, _)) => snapshot,
            Err(_) => {
                self.mark_recovery_required(store, intent).await?;
                return Err(CodexProblem::new(
                    "recovery-required",
                    Some(self.config_path()),
                ));
            }
        };
        if current.owned == intent.before().owned && current.unrelated == intent.before().unrelated
        {
            return store
                .set_recovery_state(intent.id(), RecoveryState::RolledBack)
                .await
                .map_err(|_| CodexProblem::new("recovery-required", Some(self.config_path())));
        }
        if owned_semantically_matches(&current.owned, &intent.desired().owned)
            && current.unrelated == intent.before().unrelated
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
        match self.read_snapshot() {
            Ok((current, _)) => {
                current.owned == before.owned && current.unrelated == before.unrelated
            }
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
        let (current, mut document) = self.read_snapshot()?;
        if current != *expected {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(self.config_path()),
            ));
        }
        apply_owned(&mut document, owned, preserve_decor).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(self.config_path()))
        })?;
        self.file
            .replace(
                &expected.identity,
                document.to_string().as_bytes(),
                remove_file,
            )
            .map_err(|error| map_file_error(error, Some(self.config_path())))
    }

    fn read_snapshot(&self) -> Result<(ConfigSnapshot, DocumentMut), CodexProblem> {
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
        let owned = capture_owned(&document)?;
        let unrelated = unrelated_projection(&document)?;
        Ok((
            ConfigSnapshot {
                identity: contents.identity,
                owned,
                unrelated,
            },
            document,
        ))
    }
}

fn map_file_error(error: ManagedFileError, path: Option<&Path>) -> CodexProblem {
    let code = match error {
        ManagedFileError::WriteFailed => "configuration-write-failed",
        ManagedFileError::RecoveryRequired => "recovery-required",
    };
    CodexProblem::new(code, path)
}

fn capture_owned(document: &DocumentMut) -> Result<OwnedCodexState, CodexProblem> {
    for key in ["model", "model_provider"] {
        if document.get(key).is_some_and(|item| !item.is_value()) {
            return Err(CodexProblem::new("configuration-collision", None));
        }
    }
    Ok(OwnedCodexState {
        model: item_text(document.get("model")),
        model_provider: item_text(document.get("model_provider")),
        provider_name: nested_item_text(document, "name"),
        provider_base_url: nested_item_text(document, "base_url"),
        provider_wire_api: nested_item_text(document, "wire_api"),
        provider_http_headers: nested_item_text(document, "http_headers"),
        provider_supports_websockets: nested_item_text(document, "supports_websockets"),
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

fn nested_item_text(document: &DocumentMut, key: &str) -> Option<OwnedItem> {
    document
        .get("model_providers")?
        .get("muxvia_codex")?
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
    item_semantically_matches(&left.model, &right.model)
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
    item_semantically_matches(&left.model, &right.model)
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
        semantic(&owned.model),
        semantic(&owned.model_provider),
        semantic(&owned.provider_name),
        semantic(&owned.provider_base_url),
        semantic(&owned.provider_wire_api),
        semantic(&owned.provider_http_headers),
        semantic(&owned.provider_supports_websockets),
    ))
}

fn unrelated_projection(document: &DocumentMut) -> Result<serde_json::Value, CodexProblem> {
    let mut clone = document.clone();
    clone.remove("model");
    clone.remove("model_provider");
    if let Some(providers) = clone
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        if let Some(provider) = providers
            .get_mut("muxvia_codex")
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
            .get("muxvia_codex")
            .and_then(Item::as_table_like)
            .is_some_and(|provider| provider.is_empty())
        {
            providers.remove("muxvia_codex");
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

fn apply_owned(
    document: &mut DocumentMut,
    owned: &OwnedCodexState,
    preserve_decor: bool,
) -> Result<(), ()> {
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
    if fields.iter().any(|(_, value)| value.is_some()) {
        if document.get("model_providers").is_none() {
            document["model_providers"] = Item::Table(Table::new());
        }
        let providers = document["model_providers"].as_table_mut().ok_or(())?;
        if providers.get("muxvia_codex").is_none() {
            providers["muxvia_codex"] = Item::Table(Table::new());
        }
        let provider = providers["muxvia_codex"].as_table_mut().ok_or(())?;
        for (key, encoded) in fields {
            if let Some(encoded) = encoded {
                set_table_item(provider, key, encoded, preserve_decor)?;
            } else {
                provider.remove(key);
            }
        }
    } else if let Some(providers) = document
        .get_mut("model_providers")
        .and_then(Item::as_table_like_mut)
    {
        providers.remove("muxvia_codex");
        if providers.is_empty() {
            document.remove("model_providers");
        }
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
    use std::fs;
    use tempfile::TempDir;

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
}
