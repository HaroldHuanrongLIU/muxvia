use std::{
    cell::Cell,
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    os::fd::AsFd,
    path::{Path, PathBuf},
    sync::Arc,
};

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use toml_edit::{DocumentMut, Item, Table, value};

use super::CodexProblem;
use crate::state::{RecoveryIntent, RecoveryState, StateStore};

type PreRenameHook = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
type ExchangeHook = Arc<dyn Fn(&Path, &Path) -> io::Result<bool> + Send + Sync>;

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
    mode: Option<u32>,
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
    exchange_hook: Option<ExchangeHook>,
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
            exchange_hook,
        })
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn inspect(&self) -> Result<ConfigSnapshot, CodexProblem> {
        let (snapshot, document) = self.read_snapshot()?;
        if document
            .get("model_providers")
            .and_then(|providers| providers.get("muxvia_codex"))
            .is_some()
        {
            return Err(CodexProblem::new(
                "configuration-collision",
                Some(&self.config_path),
            ));
        }
        Ok(snapshot)
    }

    pub fn inspect_managed(
        &self,
        expected: &DesiredCodexState,
    ) -> Result<ConfigSnapshot, CodexProblem> {
        let (snapshot, _) = self.read_snapshot()?;
        if !owned_semantically_matches(&snapshot.owned, &expected.0) {
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
        let header = serde_json::to_string(routing_credential)
            .expect("serializing a routing credential string cannot fail");
        DesiredCodexState(OwnedCodexState {
            model: desired_item(value(model)),
            model_provider: desired_item(value("muxvia_codex")),
            provider_name: desired_item(value("Muxvia")),
            provider_base_url: desired_item(value(base_url)),
            provider_wire_api: desired_item(value("responses")),
            provider_http_headers: parse_owned_item(&format!(
                " {{ \"X-Muxvia-Routing-Credential\" = {header} }}"
            )),
            provider_supports_websockets: desired_item(value(false)),
        })
    }

    pub fn atomic_apply(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        self.write_owned(before, &desired.0, false, true)?;
        self.verify(before, desired)
    }

    pub fn restore(
        &self,
        before: &ConfigSnapshot,
        expected_current: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        let current = self.read_snapshot()?.0;
        if !owned_semantically_matches(&current.owned, &expected_current.0) {
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
        self.write_owned(&current, &before.owned, remove_file, false)?;
        let restored = self.read_snapshot()?.0;
        if restored.owned != before.owned || restored.unrelated != current.unrelated {
            return Err(CodexProblem::new(
                "recovery-required",
                Some(&self.config_path),
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
                Some(&self.config_path),
            ))
        }
    }

    pub fn verify(
        &self,
        before: &ConfigSnapshot,
        desired: &DesiredCodexState,
    ) -> Result<(), CodexProblem> {
        let current = self.read_snapshot()?.0;
        if !owned_semantically_matches(&current.owned, &desired.0)
            || current.unrelated != before.unrelated
        {
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
        if owned_semantically_matches(&current.owned, &intent.desired().0)
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
        preserve_decor: bool,
    ) -> Result<(), CodexProblem> {
        let (current, mut document) = self.read_snapshot()?;
        if current != *expected {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(&self.config_path),
            ));
        }
        apply_owned(&mut document, owned, preserve_decor).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        let parent = self.config_path.parent().ok_or_else(|| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        create_private_parent(parent).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        let directory = rustix::fs::openat(
            rustix::fs::CWD,
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| CodexProblem::new("configuration-write-failed", Some(&self.config_path)))?;
        let directory_identity = directory_identity(&directory).map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        if !path_matches_directory(parent, directory_identity) {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(&self.config_path),
            ));
        }
        let target_name = self.config_path.file_name().ok_or_else(|| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        if file_identity_at(&directory, target_name)? != expected.identity {
            return Err(CodexProblem::new(
                "configuration-write-failed",
                Some(&self.config_path),
            ));
        }
        let temporary_name = format!(".muxvia-{}.tmp", uuid::Uuid::new_v4());
        let temporary_fd = rustix::fs::openat(
            &directory,
            temporary_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(expected.identity.mode.unwrap_or(0o600) as _),
        )
        .map_err(|_| CodexProblem::new("configuration-write-failed", Some(&self.config_path)))?;
        let mut temporary = File::from(temporary_fd);
        rustix::fs::fchmod(
            &temporary,
            Mode::from_raw_mode(expected.identity.mode.unwrap_or(0o600) as _),
        )
        .map_err(|_| CodexProblem::new("configuration-write-failed", Some(&self.config_path)))?;
        let retain_temporary = Cell::new(false);
        let result = (|| -> Result<(), CodexProblem> {
            temporary
                .write_all(document.to_string().as_bytes())
                .and_then(|_| temporary.flush())
                .and_then(|_| temporary.sync_all())
                .map_err(|_| {
                    CodexProblem::new("configuration-write-failed", Some(&self.config_path))
                })?;
            if let Some(hook) = &self.pre_rename_hook {
                hook(&parent.join(&temporary_name)).map_err(|_| {
                    CodexProblem::new("configuration-write-failed", Some(&self.config_path))
                })?;
            }
            if !path_matches_directory(parent, directory_identity)
                || file_identity_at(&directory, target_name)? != expected.identity
            {
                return Err(CodexProblem::new(
                    "configuration-write-failed",
                    Some(&self.config_path),
                ));
            }
            if expected.identity.exists {
                rustix::fs::renameat_with(
                    &directory,
                    temporary_name.as_str(),
                    &directory,
                    target_name,
                    RenameFlags::EXCHANGE,
                )
                .map_err(|_| {
                    CodexProblem::new("configuration-write-failed", Some(&self.config_path))
                })?;
                retain_temporary.set(true);
                let inject_rollback_failure = self
                    .exchange_hook
                    .as_ref()
                    .map(|hook| hook(&parent.join(&temporary_name), &self.config_path))
                    .transpose()
                    .map_err(|_| {
                        retain_temporary.set(true);
                        CodexProblem::new("recovery-required", Some(&self.config_path))
                    })?
                    .unwrap_or(false);
                let displaced_matches = file_identity_at(&directory, temporary_name.as_str())
                    .is_ok_and(|identity| identity == expected.identity);
                if !displaced_matches {
                    let rolled_back = !inject_rollback_failure
                        && rustix::fs::renameat_with(
                            &directory,
                            temporary_name.as_str(),
                            &directory,
                            target_name,
                            RenameFlags::EXCHANGE,
                        )
                        .is_ok();
                    if !rolled_back {
                        return Err(CodexProblem::new(
                            "recovery-required",
                            Some(&self.config_path),
                        ));
                    }
                    retain_temporary.set(false);
                    return Err(CodexProblem::new(
                        "configuration-write-failed",
                        Some(&self.config_path),
                    ));
                }
                retain_temporary.set(false);
                if remove_file {
                    rustix::fs::unlinkat(&directory, target_name, AtFlags::empty()).map_err(
                        |_| {
                            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
                        },
                    )?;
                }
            } else {
                rustix::fs::renameat_with(
                    &directory,
                    temporary_name.as_str(),
                    &directory,
                    target_name,
                    RenameFlags::NOREPLACE,
                )
                .map_err(|_| {
                    CodexProblem::new("configuration-write-failed", Some(&self.config_path))
                })?;
            }
            rustix::fs::fsync(&directory).map_err(|_| {
                CodexProblem::new("configuration-write-failed", Some(&self.config_path))
            })
        })();
        if !retain_temporary.get() {
            let _ = rustix::fs::unlinkat(&directory, temporary_name.as_str(), AtFlags::empty());
        }
        result
    }

    fn read_snapshot(&self) -> Result<(ConfigSnapshot, DocumentMut), CodexProblem> {
        let parent = self.config_path.parent().ok_or_else(|| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        let directory = match rustix::fs::openat(
            rustix::fs::CWD,
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => Some(directory),
            Err(error) if error == rustix::io::Errno::NOENT => None,
            Err(_) => {
                return Err(CodexProblem::new(
                    "configuration-write-failed",
                    Some(&self.config_path),
                ));
            }
        };
        let (identity, source) = if let Some(directory) = directory {
            let parent_identity = directory_identity(&directory).map_err(|_| {
                CodexProblem::new("configuration-write-failed", Some(&self.config_path))
            })?;
            if !path_matches_directory(parent, parent_identity) {
                return Err(CodexProblem::new(
                    "configuration-write-failed",
                    Some(&self.config_path),
                ));
            }
            let target_name = self.config_path.file_name().ok_or_else(|| {
                CodexProblem::new("configuration-write-failed", Some(&self.config_path))
            })?;
            let identity = file_identity_at(&directory, target_name)?;
            let source = if identity.exists {
                let file = rustix::fs::openat(
                    &directory,
                    target_name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| {
                    CodexProblem::new("configuration-write-failed", Some(&self.config_path))
                })?;
                let mut file = File::from(file);
                let mut source = String::new();
                file.read_to_string(&mut source).map_err(|_| {
                    CodexProblem::new("configuration-write-failed", Some(&self.config_path))
                })?;
                if file_identity_from_stat(rustix::fs::fstat(&file).map_err(|_| {
                    CodexProblem::new("configuration-write-failed", Some(&self.config_path))
                })?) != identity
                {
                    return Err(CodexProblem::new(
                        "configuration-write-failed",
                        Some(&self.config_path),
                    ));
                }
                source
            } else {
                String::new()
            };
            (identity, source)
        } else {
            (missing_file_identity(), String::new())
        };
        let document = source.parse::<DocumentMut>().map_err(|_| {
            CodexProblem::new("configuration-write-failed", Some(&self.config_path))
        })?;
        let owned = capture_owned(&document)?;
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

fn missing_file_identity() -> FileIdentity {
    FileIdentity {
        exists: false,
        device: None,
        inode: None,
        modified_seconds: None,
        modified_nanoseconds: None,
        length: None,
        mode: None,
    }
}

fn directory_identity(directory: &impl AsFd) -> io::Result<(u64, u64)> {
    let stat = rustix::fs::fstat(directory).map_err(io::Error::from)?;
    Ok((stat_device(&stat), stat.st_ino))
}

fn path_matches_directory(parent: &Path, expected: (u64, u64)) -> bool {
    #[cfg(unix)]
    {
        fs::symlink_metadata(parent).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink()
                && metadata.is_dir()
                && (metadata.dev(), metadata.ino()) == expected
        })
    }
    #[cfg(not(unix))]
    {
        let _ = expected;
        parent.is_dir()
    }
}

fn file_identity_at(
    directory: &impl AsFd,
    name: impl rustix::path::Arg,
) -> Result<FileIdentity, CodexProblem> {
    let stat = match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(error) if error == rustix::io::Errno::NOENT => {
            return Ok(missing_file_identity());
        }
        Err(_) => return Err(CodexProblem::new("configuration-write-failed", None)),
    };
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(CodexProblem::new("configuration-write-failed", None));
    }
    Ok(file_identity_from_stat(stat))
}

fn file_identity_from_stat(stat: rustix::fs::Stat) -> FileIdentity {
    FileIdentity {
        exists: true,
        device: Some(stat_device(&stat)),
        inode: Some(stat.st_ino),
        modified_seconds: u64::try_from(stat.st_mtime).ok(),
        modified_nanoseconds: u32::try_from(stat.st_mtime_nsec).ok(),
        length: u64::try_from(stat.st_size).ok(),
        mode: Some(permission_bits(stat.st_mode)),
    }
}

#[cfg(target_os = "macos")]
fn permission_bits(mode: rustix::fs::RawMode) -> u32 {
    u32::from(mode) & 0o777
}

#[cfg(target_os = "linux")]
fn permission_bits(mode: rustix::fs::RawMode) -> u32 {
    mode & 0o777
}

#[cfg(target_os = "macos")]
fn stat_device(stat: &rustix::fs::Stat) -> u64 {
    stat.st_dev as u64
}

#[cfg(target_os = "linux")]
fn stat_device(stat: &rustix::fs::Stat) -> u64 {
    stat.st_dev
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
    fn equal(left: &Option<OwnedItem>, right: &Option<OwnedItem>) -> bool {
        left.as_ref().map(|item| &item.semantic) == right.as_ref().map(|item| &item.semantic)
    }
    equal(&left.model, &right.model)
        && equal(&left.model_provider, &right.model_provider)
        && equal(&left.provider_name, &right.provider_name)
        && equal(&left.provider_base_url, &right.provider_base_url)
        && equal(&left.provider_wire_api, &right.provider_wire_api)
        && equal(&left.provider_http_headers, &right.provider_http_headers)
        && equal(
            &left.provider_supports_websockets,
            &right.provider_supports_websockets,
        )
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

fn create_private_parent(parent: &Path) -> io::Result<()> {
    let existed = parent.exists();
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    if !existed {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::permission_bits;
    use rustix::fs::Mode;

    #[test]
    fn permission_bits_project_to_portable_u32() {
        let raw_mode = (Mode::RUSR | Mode::WUSR | Mode::RGRP).as_raw_mode();

        assert_eq!(permission_bits(raw_mode), 0o640);
    }
}
