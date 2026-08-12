use std::{
    fmt,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, value};

use super::CodexProblem;
use crate::state::{RecoveryIntent, RecoveryState, StateStore};

type PreRenameHook = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnedCodexState {
    model: Option<serde_json::Value>,
    model_provider: Option<serde_json::Value>,
    provider_name: Option<serde_json::Value>,
    provider_base_url: Option<serde_json::Value>,
    provider_wire_api: Option<serde_json::Value>,
    provider_http_headers: Option<serde_json::Value>,
    provider_supports_websockets: Option<serde_json::Value>,
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

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesiredCodexState(OwnedCodexState);

impl fmt::Debug for DesiredCodexState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DesiredCodexState(<redacted>)")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileIdentity {
    exists: bool,
    device: Option<u64>,
    inode: Option<u64>,
    modified_seconds: Option<u64>,
    modified_nanoseconds: Option<u32>,
    length: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigSnapshot {
    identity: FileIdentity,
    owned: OwnedCodexState,
    unrelated: serde_json::Value,
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
}

pub struct CodexConfigCodec {
    config_path: PathBuf,
    pre_rename_hook: Option<PreRenameHook>,
}

impl CodexConfigCodec {
    pub fn for_user_home(user_home: &Path) -> Result<Self, CodexProblem> {
        Self::build(user_home, None)
    }

    pub fn for_user_home_with_pre_rename_hook(
        user_home: &Path,
        hook: PreRenameHook,
    ) -> Result<Self, CodexProblem> {
        Self::build(user_home, Some(hook))
    }

    fn build(user_home: &Path, hook: Option<PreRenameHook>) -> Result<Self, CodexProblem> {
        let configured_home = user_home.join(".codex");
        let config_home = match fs::symlink_metadata(&configured_home) {
            Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(&configured_home)
                .map_err(|_| {
                    CodexProblem::new("configuration-write-failed", Some(&configured_home))
                })?,
            Ok(_) => configured_home,
            Err(error) if error.kind() == io::ErrorKind::NotFound => configured_home,
            Err(_) => {
                return Err(CodexProblem::new(
                    "configuration-write-failed",
                    Some(&configured_home),
                ));
            }
        };
        Ok(Self {
            config_path: config_home.join("config.toml"),
            pre_rename_hook: hook,
        })
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn inspect(&self) -> Result<ConfigSnapshot, CodexProblem> {
        let (snapshot, document) = self.read_snapshot()?;
        if reserved_provider_is_collision(&document) {
            return Err(CodexProblem::new(
                "configuration-collision",
                Some(&self.config_path),
            ));
        }
        Ok(snapshot)
    }

    pub fn desired(
        &self,
        model: &str,
        base_url: &str,
        routing_credential: &str,
    ) -> DesiredCodexState {
        let mut headers = serde_json::Map::new();
        headers.insert(
            "X-Muxvia-Routing-Credential".to_owned(),
            serde_json::Value::String(routing_credential.to_owned()),
        );
        DesiredCodexState(OwnedCodexState {
            model: Some(model.into()),
            model_provider: Some("muxvia_codex".into()),
            provider_name: Some("Muxvia".into()),
            provider_base_url: Some(base_url.into()),
            provider_wire_api: Some("responses".into()),
            provider_http_headers: Some(serde_json::Value::Object(headers)),
            provider_supports_websockets: Some(false.into()),
        })
    }

    pub fn atomic_apply(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        self.write_owned(before, &desired.0, false)?;
        self.verify(before, desired)
    }

    pub fn restore(
        &self,
        before: &ConfigSnapshot,
        expected_current: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        let current = self.read_snapshot()?.0;
        if current.owned != expected_current.0 {
            return Err(CodexProblem::new(
                "recovery-required",
                Some(&self.config_path),
            ));
        }
        let remove_file = !before.identity.exists
            && current
                .unrelated
                .as_object()
                .is_some_and(serde_json::Map::is_empty);
        self.write_owned(&current, &before.owned, remove_file)?;
        let restored = self.read_snapshot()?.0;
        if restored.owned != before.owned || restored.unrelated != current.unrelated {
            return Err(CodexProblem::new(
                "recovery-required",
                Some(&self.config_path),
            ));
        }
        Ok(())
    }

    pub fn verify(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        let current = self.read_snapshot()?.0;
        if current.owned != desired.0 || current.unrelated != before.unrelated {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(&self.config_path),
            ));
        }
        Ok(())
    }

    pub async fn reconcile_pending(&self, store: &StateStore) -> Result<(), CodexProblem> {
        for intent in store
            .pending_recovery_intents()
            .await
            .map_err(|_| CodexProblem::new("recovery-required", Some(&self.config_path)))?
        {
            self.reconcile_one(store, &intent).await?;
        }
        Ok(())
    }

    async fn reconcile_one(
        &self,
        store: &StateStore,
        intent: &RecoveryIntent,
    ) -> Result<(), CodexProblem> {
        if intent.config_path() != self.config_path {
            store
                .set_recovery_state(intent.id(), RecoveryState::RecoveryRequired)
                .await
                .map_err(|_| CodexProblem::new("recovery-required", Some(&self.config_path)))?;
            return Err(CodexProblem::new(
                "recovery-required",
                Some(&self.config_path),
            ));
        }
        let current = match self.read_snapshot() {
            Ok((snapshot, _)) => snapshot,
            Err(_) => {
                self.mark_recovery_required(store, intent).await?;
                return Err(CodexProblem::new(
                    "recovery-required",
                    Some(&self.config_path),
                ));
            }
        };
        if current.owned == intent.before().owned && current.unrelated == intent.before().unrelated
        {
            return store
                .set_recovery_state(intent.id(), RecoveryState::RolledBack)
                .await
                .map_err(|_| CodexProblem::new("recovery-required", Some(&self.config_path)));
        }
        if current.owned == intent.desired().0
            && current.unrelated == intent.before().unrelated
            && self.restore(intent.before(), intent.desired()).is_ok()
            && self.matches_before(intent.before())
        {
            return store
                .set_recovery_state(intent.id(), RecoveryState::RolledBack)
                .await
                .map_err(|_| CodexProblem::new("recovery-required", Some(&self.config_path)));
        }
        self.mark_recovery_required(store, intent).await?;
        Err(CodexProblem::new(
            "recovery-required",
            Some(&self.config_path),
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
            .map_err(|_| CodexProblem::new("recovery-required", Some(&self.config_path)))
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
    ) -> Result<(), CodexProblem> {
        let (current, mut document) = self.read_snapshot()?;
        if current != *expected {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(&self.config_path),
            ));
        }
        apply_owned(&mut document, owned).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        let parent = self.config_path.parent().ok_or_else(|| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        create_private_parent(parent).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;

        let mut temporary = NamedTempFile::new_in(parent).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        set_mode(temporary.path(), mode_for(&self.config_path)).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        temporary
            .write_all(document.to_string().as_bytes())
            .and_then(|_| temporary.flush())
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|_| {
                CodexProblem::new("configuration-write-failed", Some(&self.config_path))
            })?;
        if let Some(hook) = &self.pre_rename_hook {
            hook(temporary.path()).map_err(|_| {
                CodexProblem::new("configuration-write-failed", Some(&self.config_path))
            })?;
        }
        let rechecked = self.read_snapshot()?.0;
        if rechecked != *expected {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(&self.config_path),
            ));
        }
        if remove_file {
            drop(temporary);
            fs::remove_file(&self.config_path).map_err(|_| {
                CodexProblem::new("configuration-write-failed", Some(&self.config_path))
            })?;
        } else {
            temporary.persist(&self.config_path).map_err(|_| {
                CodexProblem::new("configuration-write-failed", Some(&self.config_path))
            })?;
        }
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| {
                CodexProblem::new("configuration-write-failed", Some(&self.config_path))
            })?;
        Ok(())
    }

    fn read_snapshot(&self) -> Result<(ConfigSnapshot, DocumentMut), CodexProblem> {
        let identity = file_identity(&self.config_path)?;
        let source = if identity.exists {
            fs::read_to_string(&self.config_path).map_err(|_| {
                CodexProblem::new("configuration-write-failed", Some(&self.config_path))
            })?
        } else {
            String::new()
        };
        let document = source.parse::<DocumentMut>().map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        let owned = capture_owned(&document);
        let unrelated = unrelated_projection(&document)?;
        Ok((
            ConfigSnapshot {
                identity,
                owned,
                unrelated,
            },
            document,
        ))
    }
}

fn file_identity(path: &Path) -> Result<FileIdentity, CodexProblem> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(FileIdentity {
                exists: false,
                device: None,
                inode: None,
                modified_seconds: None,
                modified_nanoseconds: None,
                length: None,
            });
        }
        Err(_) => return Err(CodexProblem::new("configuration-write-failed", Some(path))),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CodexProblem::new("configuration-write-failed", Some(path)));
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    #[cfg(unix)]
    let (device, inode) = (Some(metadata.dev()), Some(metadata.ino()));
    #[cfg(not(unix))]
    let (device, inode) = (None, None);
    Ok(FileIdentity {
        exists: true,
        device,
        inode,
        modified_seconds: modified.map(|duration| duration.as_secs()),
        modified_nanoseconds: modified.map(|duration| duration.subsec_nanos()),
        length: Some(metadata.len()),
    })
}

fn capture_owned(document: &DocumentMut) -> OwnedCodexState {
    OwnedCodexState {
        model: item_text(document.get("model")),
        model_provider: item_text(document.get("model_provider")),
        provider_name: nested_item_text(document, "name"),
        provider_base_url: nested_item_text(document, "base_url"),
        provider_wire_api: nested_item_text(document, "wire_api"),
        provider_http_headers: nested_item_text(document, "http_headers"),
        provider_supports_websockets: nested_item_text(document, "supports_websockets"),
    }
}

fn item_text(item: Option<&Item>) -> Option<serde_json::Value> {
    item.filter(|item| !item.is_none())
        .and_then(|item| item.as_value())
        .and_then(value_semantic)
}

fn nested_item_text(document: &DocumentMut, key: &str) -> Option<serde_json::Value> {
    document
        .get("model_providers")?
        .get("muxvia_codex")?
        .get(key)
        .filter(|item| !item.is_none())
        .and_then(Item::as_value)
        .and_then(value_semantic)
}

fn value_semantic(value: &toml_edit::Value) -> Option<serde_json::Value> {
    let document = format!("owned = {value}\n").parse::<DocumentMut>().ok()?;
    let mut tree: serde_json::Value = toml_edit::de::from_document(document).ok()?;
    tree.as_object_mut()?.remove("owned")
}

fn reserved_provider_is_collision(document: &DocumentMut) -> bool {
    let Some(provider) = document
        .get("model_providers")
        .and_then(|providers| providers.get("muxvia_codex"))
    else {
        return false;
    };
    provider
        .get("name")
        .and_then(Item::as_str)
        .is_none_or(|name| name != "Muxvia")
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

fn apply_owned(document: &mut DocumentMut, owned: &OwnedCodexState) -> Result<(), ()> {
    set_top_level(document, "model", owned.model.as_ref())?;
    set_top_level(document, "model_provider", owned.model_provider.as_ref())?;
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
                provider[key] = parse_value(encoded)?;
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
    encoded: Option<&serde_json::Value>,
) -> Result<(), ()> {
    if let Some(encoded) = encoded {
        document[key] = parse_value(encoded)?;
    } else {
        document.remove(key);
    }
    Ok(())
}

fn parse_value(encoded: &serde_json::Value) -> Result<Item, ()> {
    match encoded {
        serde_json::Value::String(text) => Ok(value(text)),
        serde_json::Value::Bool(flag) => Ok(value(*flag)),
        serde_json::Value::Object(entries) => {
            let body = entries
                .iter()
                .map(|(key, value)| {
                    let text = value.as_str()?;
                    Some(format!(
                        "{} = {}",
                        serde_json::to_string(key).ok()?,
                        serde_json::to_string(text).ok()?
                    ))
                })
                .collect::<Option<Vec<_>>>()
                .ok_or(())?
                .join(", ");
            let document = format!("owned = {{ {body} }}\n")
                .parse::<DocumentMut>()
                .map_err(|_| ())?;
            document.get("owned").cloned().ok_or(())
        }
        _ => Err(()),
    }
}

fn create_private_parent(parent: &Path) -> io::Result<()> {
    let existed = parent.exists();
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    if !existed {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn mode_for(path: &Path) -> u32 {
    #[cfg(unix)]
    if let Ok(metadata) = fs::metadata(path) {
        return metadata.permissions().mode() & 0o777;
    }
    0o600
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}
