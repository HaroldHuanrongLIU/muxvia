use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use ring::digest::{Context, SHA256, digest};
use rustix::fs::RenameFlags;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    claude::ClaudeConfigCodec,
    codex::CodexConfigCodec,
    config::managed_file::{ManagedFile, ManagedFileContents},
    control::protocol::{
        ClaudePreflightContext, RecoveryBackupCompatibility, RecoveryBackupEntryKind,
        RecoveryBackupEntrySummary, RecoveryBackupInspection, Target,
    },
    home::MuxviaHome,
    state::{SCHEMA_VERSION, StateStore},
    subscription::{
        DeviceAuthorizationManager, SubscriptionAccountCoordinator, SubscriptionAccountStore,
    },
};

use super::{activate::ActivationService, reconcile::ReconciliationService};

const MAGIC: &[u8] = b"MUXVIA-RECOVERY-V1\n";
const FORMAT: &str = "muxvia-recovery-backup";
const FORMAT_VERSION: u32 = 1;
const PRIVATE_FILE_MODE: u32 = 0o600;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_RELEASE_BYTES: usize = 256;
const MAX_NON_DATABASE_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
const ENTRY_KINDS: [RecoveryBackupEntryKind; 4] = [
    RecoveryBackupEntryKind::SqliteState,
    RecoveryBackupEntryKind::SubscriptionAccounts,
    RecoveryBackupEntryKind::CodexManagedConfiguration,
    RecoveryBackupEntryKind::ClaudeManagedConfiguration,
];

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RecoveryBackupError {
    #[error("Recovery Backup path is invalid")]
    InvalidPath,
    #[error("Recovery Backup permissions are unsafe")]
    UnsafePermissions,
    #[error("Recovery Backup is invalid")]
    InvalidArtifact,
    #[error("Recovery Snapshot changed while it was captured")]
    SnapshotChanged,
    #[error("Recovery Backup could not be created")]
    CreationFailed,
    #[error("Recovery Backup is not compatible with this release")]
    Incompatible,
    #[error("Recovery Backup preflight failed")]
    PreflightFailed,
    #[error("Recovery Backup restore step failed at {stage}")]
    RestoreStepFailed { stage: &'static str },
    #[error("Recovery Backup restore failed and the prior installation was recovered")]
    RestoreFailed {
        pre_restore_backup_path: PathBuf,
        stage: &'static str,
    },
    #[error("Recovery Backup restore requires manual recovery")]
    RecoveryRequired {
        pre_restore_backup_path: PathBuf,
        stage: &'static str,
    },
}

pub(crate) struct CreatedRecoveryBackup {
    pub(crate) path: PathBuf,
    pub(crate) inspection: RecoveryBackupInspection,
}

pub(crate) struct RestoredRecoveryBackup {
    pub(crate) restored_snapshot_id: Uuid,
    pub(crate) pre_restore_snapshot_id: Uuid,
    pub(crate) pre_restore_backup_path: PathBuf,
    pub(crate) resumed_takeovers: Vec<crate::control::protocol::Target>,
}

type BeforeInstallHook = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
type RestoreHook = Arc<dyn Fn(RecoveryRestoreStage) -> io::Result<()> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryRestoreStage {
    AfterDatabase,
    AfterAccounts,
    AfterCodex,
    AfterClaude,
    BeforeRuntimeResume,
    Verification,
    BeforeRollback,
}

const fn restore_stage_name(stage: RecoveryRestoreStage) -> &'static str {
    match stage {
        RecoveryRestoreStage::AfterDatabase => "database",
        RecoveryRestoreStage::AfterAccounts => "subscription-accounts",
        RecoveryRestoreStage::AfterCodex => "codex-configuration",
        RecoveryRestoreStage::AfterClaude => "claude-configuration",
        RecoveryRestoreStage::BeforeRuntimeResume => "runtime",
        RecoveryRestoreStage::Verification => "verification",
        RecoveryRestoreStage::BeforeRollback => "rollback",
    }
}

#[derive(Default)]
struct RecoveryBackupHooks {
    before_install: Option<BeforeInstallHook>,
    restore: Option<RestoreHook>,
}

pub(crate) struct RecoveryBackupService {
    store: Arc<StateStore>,
    home: MuxviaHome,
    reconciliation: Arc<ReconciliationService>,
    activation: Arc<ActivationService>,
    accounts: Arc<SubscriptionAccountStore>,
    account_coordinator: Arc<SubscriptionAccountCoordinator>,
    device_authorization: Arc<DeviceAuthorizationManager>,
    release: String,
    creation_gate: Mutex<()>,
    hooks: RecoveryBackupHooks,
}

impl RecoveryBackupService {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Arc<StateStore>,
        home: MuxviaHome,
        reconciliation: Arc<ReconciliationService>,
        activation: Arc<ActivationService>,
        accounts: Arc<SubscriptionAccountStore>,
        account_coordinator: Arc<SubscriptionAccountCoordinator>,
        device_authorization: Arc<DeviceAuthorizationManager>,
        release: String,
    ) -> Result<Self, RecoveryBackupError> {
        if release.is_empty()
            || release.len() > MAX_RELEASE_BYTES
            || !release
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        {
            return Err(RecoveryBackupError::CreationFailed);
        }
        Ok(Self {
            store,
            home,
            reconciliation,
            activation,
            accounts,
            account_coordinator,
            device_authorization,
            release,
            creation_gate: Mutex::new(()),
            hooks: RecoveryBackupHooks::default(),
        })
    }

    #[cfg(test)]
    fn with_before_install_hook(mut self, hook: BeforeInstallHook) -> Self {
        self.hooks.before_install = Some(hook);
        self
    }

    #[cfg(test)]
    fn with_restore_hook(mut self, hook: RestoreHook) -> Self {
        self.hooks.restore = Some(hook);
        self
    }

    pub(crate) async fn create(&self) -> Result<CreatedRecoveryBackup, RecoveryBackupError> {
        let _creation = self.creation_gate.lock().await;
        let _codex = self
            .reconciliation
            .lock_target_mutation(crate::control::protocol::Target::Codex)
            .await;
        let _claude = self
            .reconciliation
            .lock_target_mutation(crate::control::protocol::Target::Claude)
            .await;
        let _accounts = self.account_coordinator.lock_recovery_snapshot().await;

        self.create_locked().await
    }

    async fn create_locked(&self) -> Result<CreatedRecoveryBackup, RecoveryBackupError> {
        let backups = self
            .home
            .prepare_backups_dir()
            .map_err(|_| RecoveryBackupError::CreationFailed)?;
        let snapshot_id = Uuid::new_v4();
        let database_path = backups.join(format!(".recovery-{snapshot_id}.sqlite.pending"));
        let database_guard = PendingFile::new(database_path.clone());
        create_private_empty_file(&database_path)?;

        let account_file = self
            .accounts
            .recovery_file_contents()
            .map_err(|_| RecoveryBackupError::CreationFailed)?;
        let codex_file = managed_file_contents(self.home.user_home(), ".codex", "config.toml")?;
        let claude_file = managed_file_contents(self.home.user_home(), ".claude", "settings.json")?;

        let database_schema_version = self
            .store
            .create_online_backup(database_path.clone())
            .await
            .map_err(|_| RecoveryBackupError::CreationFailed)?;
        set_private_permissions(&database_path)?;

        if account_file
            != self
                .accounts
                .recovery_file_contents()
                .map_err(|_| RecoveryBackupError::SnapshotChanged)?
            || codex_file != managed_file_contents(self.home.user_home(), ".codex", "config.toml")?
            || claude_file
                != managed_file_contents(self.home.user_home(), ".claude", "settings.json")?
        {
            return Err(RecoveryBackupError::SnapshotChanged);
        }

        let entries = vec![
            CapturedEntry::from_path(
                RecoveryBackupEntryKind::SqliteState,
                database_path.clone(),
                PRIVATE_FILE_MODE,
            )?,
            CapturedEntry::from_contents(
                RecoveryBackupEntryKind::SubscriptionAccounts,
                account_file,
            ),
            CapturedEntry::from_contents(
                RecoveryBackupEntryKind::CodexManagedConfiguration,
                codex_file,
            ),
            CapturedEntry::from_contents(
                RecoveryBackupEntryKind::ClaudeManagedConfiguration,
                claude_file,
            ),
        ];
        let created_at_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RecoveryBackupError::CreationFailed)?
            .as_secs();
        let manifest = RecoveryBackupManifest {
            format: FORMAT.to_owned(),
            format_version: FORMAT_VERSION,
            sensitive: true,
            snapshot_id,
            created_at_unix_seconds,
            created_by_release: self.release.clone(),
            database_schema_version,
            entries: entries.iter().map(|entry| entry.manifest.clone()).collect(),
        };
        validate_manifest(&manifest)?;

        let pending_path = backups.join(format!(".recovery-{snapshot_id}.pending"));
        let final_path = backups.join(format!("{snapshot_id}.muxvia-recovery"));
        let mut pending_guard = PendingFile::new(pending_path.clone());
        write_container(&pending_path, &manifest, &entries)?;
        drop(database_guard);
        sync_directory(&backups)?;
        if let Some(hook) = &self.hooks.before_install {
            hook(&pending_path).map_err(|_| RecoveryBackupError::CreationFailed)?;
        }
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            &pending_path,
            rustix::fs::CWD,
            &final_path,
            RenameFlags::NOREPLACE,
        )
        .map_err(|_| RecoveryBackupError::CreationFailed)?;
        pending_guard.disarm();
        if sync_directory(&backups).is_err() {
            let _ = fs::remove_file(&final_path);
            let _ = sync_directory(&backups);
            return Err(RecoveryBackupError::CreationFailed);
        }

        let inspection = match self.inspect(&final_path) {
            Ok(inspection) => inspection,
            Err(error) => {
                let _ = fs::remove_file(&final_path);
                let _ = sync_directory(&backups);
                return Err(error);
            }
        };
        Ok(CreatedRecoveryBackup {
            path: final_path,
            inspection,
        })
    }

    pub(crate) async fn restore(
        &self,
        path: &Path,
        claude_context: Option<ClaudePreflightContext>,
    ) -> Result<RestoredRecoveryBackup, RecoveryBackupError> {
        let _creation = self.creation_gate.lock().await;
        let _codex = self
            .reconciliation
            .lock_target_mutation(crate::control::protocol::Target::Codex)
            .await;
        let _claude = self
            .reconciliation
            .lock_target_mutation(crate::control::protocol::Target::Claude)
            .await;
        let _accounts = self.account_coordinator.lock_recovery_snapshot().await;
        let backups = self
            .home
            .prepare_backups_dir()
            .map_err(|_| RecoveryBackupError::PreflightFailed)?;
        let selected = extract_artifact(path, &backups)?;
        if selected.manifest.database_schema_version > SCHEMA_VERSION {
            return Err(RecoveryBackupError::Incompatible);
        }
        let account_document = validate_recovery_snapshot(&selected)?;
        let preparation =
            StateStore::prepare_recovery_database(selected.database_path.clone(), account_document)
                .await
                .map_err(|_| RecoveryBackupError::PreflightFailed)?;
        if preparation.source_schema_version != selected.manifest.database_schema_version {
            return Err(RecoveryBackupError::PreflightFailed);
        }
        self.preflight_restore(&preparation.takeover_ports, claude_context.as_ref())
            .await?;

        let pre_restore = self.create_locked().await?;
        let rollback = extract_artifact(&pre_restore.path, &backups)?;
        if self.activation.shutdown_models().await.is_err() {
            let recovered = self.resume_takeovers_locked().await.is_ok();
            return if recovered {
                Err(RecoveryBackupError::RestoreFailed {
                    pre_restore_backup_path: pre_restore.path,
                    stage: "runtime",
                })
            } else {
                let _ = self.store.mark_recovery_restore_required().await;
                Err(RecoveryBackupError::RecoveryRequired {
                    pre_restore_backup_path: pre_restore.path,
                    stage: "runtime",
                })
            };
        }
        let port_reservations = self
            .reserve_takeover_ports(&preparation.takeover_ports)
            .await;
        let port_reservations = match port_reservations {
            Ok(reservations) => reservations,
            Err(error) => {
                let recovered = self.resume_takeovers_locked().await.is_ok();
                return if recovered {
                    Err(RecoveryBackupError::RestoreFailed {
                        pre_restore_backup_path: pre_restore.path,
                        stage: "runtime",
                    })
                } else {
                    let _ = self.store.mark_recovery_restore_required().await;
                    Err(RecoveryBackupError::RecoveryRequired {
                        pre_restore_backup_path: pre_restore.path,
                        stage: match error {
                            RecoveryBackupError::RestoreStepFailed { stage } => stage,
                            _ => "runtime",
                        },
                    })
                };
            }
        };
        self.device_authorization.reset_for_recovery_restore().await;
        let applied = async {
            self.store
                .restore_database_from(selected.database_path.clone())
                .await
                .map_err(|_| RecoveryBackupError::RestoreStepFailed { stage: "database" })?;
            self.run_restore_hook(RecoveryRestoreStage::AfterDatabase)?;
            apply_recovery_snapshot(&self.home, &selected, self.hooks.restore.as_ref())?;
            self.run_restore_hook(RecoveryRestoreStage::Verification)?;
            verify_live_snapshot(&self.home, &selected)?;
            self.run_restore_hook(RecoveryRestoreStage::BeforeRuntimeResume)?;
            drop(port_reservations);
            self.resume_takeovers_locked().await
        }
        .await;
        let resumed_takeovers = match applied {
            Ok(targets) => targets,
            Err(error) => {
                let stage = match error {
                    RecoveryBackupError::RestoreStepFailed { stage } => stage,
                    _ => "unknown",
                };
                let runtimes_stopped = self.activation.shutdown_models().await.is_ok();
                let rolled_back = self
                    .run_restore_hook(RecoveryRestoreStage::BeforeRollback)
                    .is_ok()
                    && runtimes_stopped
                    && self
                        .store
                        .restore_database_from(rollback.database_path.clone())
                        .await
                        .and_then(|_| {
                            apply_recovery_snapshot(&self.home, &rollback, None)
                                .map_err(|_| crate::state::StateError::Unavailable)
                        })
                        .is_ok()
                    && verify_live_snapshot(&self.home, &rollback).is_ok()
                    && self.resume_takeovers_locked().await.is_ok();
                if rolled_back {
                    Err(RecoveryBackupError::RestoreFailed {
                        pre_restore_backup_path: pre_restore.path.clone(),
                        stage,
                    })
                } else {
                    let _ = self.store.mark_recovery_restore_required().await;
                    Err(RecoveryBackupError::RecoveryRequired {
                        pre_restore_backup_path: pre_restore.path.clone(),
                        stage,
                    })
                }?
            }
        };
        self.store.reset_recovery_publication_tracking();

        drop(rollback);
        Ok(RestoredRecoveryBackup {
            restored_snapshot_id: selected.manifest.snapshot_id,
            pre_restore_snapshot_id: pre_restore.inspection.snapshot_id,
            pre_restore_backup_path: pre_restore.path,
            resumed_takeovers,
        })
    }

    async fn preflight_restore(
        &self,
        takeover_ports: &[(Target, u16)],
        claude_context: Option<&ClaudePreflightContext>,
    ) -> Result<(), RecoveryBackupError> {
        let (_, profile_shadow) = CodexConfigCodec::for_user_home(self.home.user_home())
            .and_then(|codec| codec.reconciliation_snapshot())
            .map_err(|_| RecoveryBackupError::PreflightFailed)?;
        if profile_shadow {
            return Err(RecoveryBackupError::PreflightFailed);
        }
        let claude_context = claude_context.ok_or(RecoveryBackupError::PreflightFailed)?;
        ClaudeConfigCodec::for_user_home(self.home.user_home())
            .and_then(|codec| codec.preflight(claude_context))
            .map_err(|_| RecoveryBackupError::PreflightFailed)?;

        let mut active_ports = Vec::new();
        for target in [Target::Codex, Target::Claude] {
            if let Some(endpoint) = self.activation.model_endpoint_for(target).await {
                active_ports.push(endpoint.port());
            }
        }
        for (target, port) in takeover_ports {
            self.activation
                .preflight_recovery_compatibility(*target)
                .map_err(|_| RecoveryBackupError::PreflightFailed)?;
            if !active_ports.contains(port) {
                TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port))
                    .await
                    .map_err(|_| RecoveryBackupError::PreflightFailed)?;
            }
        }
        Ok(())
    }

    async fn reserve_takeover_ports(
        &self,
        takeover_ports: &[(Target, u16)],
    ) -> Result<Vec<TcpListener>, RecoveryBackupError> {
        let mut reservations = Vec::with_capacity(takeover_ports.len());
        for (_, port) in takeover_ports {
            reservations.push(
                TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port))
                    .await
                    .map_err(|_| RecoveryBackupError::RestoreStepFailed { stage: "runtime" })?,
            );
        }
        Ok(reservations)
    }

    async fn resume_takeovers_locked(
        &self,
    ) -> Result<Vec<crate::control::protocol::Target>, RecoveryBackupError> {
        let mut resumed = Vec::new();
        for target in [
            crate::control::protocol::Target::Codex,
            crate::control::protocol::Target::Claude,
        ] {
            self.activation
                .bootstrap_committed_takeover_for_locked(target)
                .await
                .map_err(|_| RecoveryBackupError::RestoreStepFailed { stage: "runtime" })?;
            let takeover = self
                .store
                .committed_takeover_for(target)
                .await
                .map_err(|_| RecoveryBackupError::RestoreStepFailed { stage: "runtime" })?;
            if takeover.is_some() {
                if self.activation.model_endpoint_for(target).await.is_none() {
                    return Err(RecoveryBackupError::RestoreStepFailed { stage: "runtime" });
                }
                resumed.push(target);
            }
            self.store
                .clear_startup_problems_for(target)
                .await
                .map_err(|_| RecoveryBackupError::RestoreStepFailed { stage: "runtime" })?;
        }
        Ok(resumed)
    }

    fn run_restore_hook(&self, stage: RecoveryRestoreStage) -> Result<(), RecoveryBackupError> {
        self.hooks
            .restore
            .as_ref()
            .map(|hook| hook(stage))
            .transpose()
            .map_err(|_| RecoveryBackupError::RestoreStepFailed {
                stage: restore_stage_name(stage),
            })?;
        Ok(())
    }

    pub(crate) fn inspect(
        &self,
        path: &Path,
    ) -> Result<RecoveryBackupInspection, RecoveryBackupError> {
        inspect_artifact(path)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryBackupManifest {
    format: String,
    format_version: u32,
    sensitive: bool,
    snapshot_id: Uuid,
    created_at_unix_seconds: u64,
    created_by_release: String,
    database_schema_version: u32,
    entries: Vec<RecoveryBackupEntryManifest>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryBackupEntryManifest {
    kind: RecoveryBackupEntryKind,
    present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<u32>,
    byte_length: u64,
    sha256: String,
}

enum CapturedData {
    Path(PathBuf),
    Bytes(Vec<u8>),
}

struct CapturedEntry {
    manifest: RecoveryBackupEntryManifest,
    data: CapturedData,
}

#[derive(Clone)]
struct RecoveryFileEntry {
    present: bool,
    mode: Option<u32>,
    bytes: Vec<u8>,
}

struct ExtractedRecoverySnapshot {
    manifest: RecoveryBackupManifest,
    database_path: PathBuf,
    accounts: RecoveryFileEntry,
    codex: RecoveryFileEntry,
    claude: RecoveryFileEntry,
    _staging: PendingDirectory,
}

fn extract_artifact(
    artifact_path: &Path,
    backups: &Path,
) -> Result<ExtractedRecoverySnapshot, RecoveryBackupError> {
    let inspection = inspect_artifact(artifact_path)?;
    if inspection.compatibility == RecoveryBackupCompatibility::UnsupportedDatabaseSchema {
        return Err(RecoveryBackupError::Incompatible);
    }
    let (mut artifact, manifest) = open_container_at_content(artifact_path)?;
    if manifest.snapshot_id != inspection.snapshot_id {
        return Err(RecoveryBackupError::InvalidArtifact);
    }
    let staging_path = backups.join(format!(".restore-{}.staging", Uuid::new_v4()));
    fs::create_dir(&staging_path).map_err(|_| RecoveryBackupError::PreflightFailed)?;
    fs::set_permissions(&staging_path, fs::Permissions::from_mode(0o700))
        .map_err(|_| RecoveryBackupError::PreflightFailed)?;
    let staging = PendingDirectory::new(staging_path.clone());
    let database_path = staging_path.join("muxvia.db");
    let mut files = Vec::with_capacity(3);
    for (index, entry) in manifest.entries.iter().enumerate() {
        if index == 0 {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true).mode(PRIVATE_FILE_MODE);
            let mut output = options
                .open(&database_path)
                .map_err(|_| RecoveryBackupError::PreflightFailed)?;
            let copied = io::copy(
                &mut Read::by_ref(&mut artifact).take(entry.byte_length),
                &mut output,
            )
            .map_err(|_| RecoveryBackupError::PreflightFailed)?;
            if copied != entry.byte_length {
                return Err(RecoveryBackupError::InvalidArtifact);
            }
            output
                .flush()
                .and_then(|_| output.sync_all())
                .map_err(|_| RecoveryBackupError::PreflightFailed)?;
            set_private_permissions(&database_path)?;
            let (length, hash) = hash_file(&database_path)?;
            if length != entry.byte_length || hash != entry.sha256 {
                return Err(RecoveryBackupError::InvalidArtifact);
            }
            continue;
        }
        if entry.byte_length > MAX_NON_DATABASE_ENTRY_BYTES {
            return Err(RecoveryBackupError::InvalidArtifact);
        }
        let length =
            usize::try_from(entry.byte_length).map_err(|_| RecoveryBackupError::InvalidArtifact)?;
        let mut bytes = vec![0_u8; length];
        artifact
            .read_exact(&mut bytes)
            .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
        if sha256_hex(&bytes) != entry.sha256 {
            return Err(RecoveryBackupError::InvalidArtifact);
        }
        files.push(RecoveryFileEntry {
            present: entry.present,
            mode: entry.mode,
            bytes,
        });
    }
    sync_directory(&staging_path).map_err(|_| RecoveryBackupError::PreflightFailed)?;
    let [accounts, codex, claude]: [RecoveryFileEntry; 3] = files
        .try_into()
        .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    Ok(ExtractedRecoverySnapshot {
        manifest,
        database_path,
        accounts,
        codex,
        claude,
        _staging: staging,
    })
}

fn open_container_at_content(
    path: &Path,
) -> Result<(File, RecoveryBackupManifest), RecoveryBackupError> {
    if !path.is_absolute() || path.as_os_str().len() > MAX_PATH_BYTES {
        return Err(RecoveryBackupError::InvalidPath);
    }
    if fs::symlink_metadata(path)
        .map_err(|_| RecoveryBackupError::InvalidPath)?
        .file_type()
        .is_symlink()
    {
        return Err(RecoveryBackupError::InvalidPath);
    }
    let mut file = File::open(path).map_err(|_| RecoveryBackupError::InvalidPath)?;
    let metadata = file
        .metadata()
        .map_err(|_| RecoveryBackupError::InvalidPath)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(RecoveryBackupError::UnsafePermissions);
    }
    let mut magic = vec![0_u8; MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    if magic != MAGIC {
        return Err(RecoveryBackupError::InvalidArtifact);
    }
    let mut header_length = [0_u8; 4];
    file.read_exact(&mut header_length)
        .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    let header_length = u32::from_be_bytes(header_length) as usize;
    if header_length == 0 || header_length > MAX_HEADER_BYTES {
        return Err(RecoveryBackupError::InvalidArtifact);
    }
    let mut header = vec![0_u8; header_length];
    file.read_exact(&mut header)
        .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    let manifest =
        serde_json::from_slice(&header).map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    validate_manifest(&manifest)?;
    Ok((file, manifest))
}

fn validate_recovery_snapshot(
    snapshot: &ExtractedRecoverySnapshot,
) -> Result<crate::subscription::SubscriptionAccountDocument, RecoveryBackupError> {
    let account_document =
        crate::subscription::SubscriptionAccountStore::validate_recovery_contents(
            snapshot.accounts.present,
            snapshot.accounts.mode,
            &snapshot.accounts.bytes,
        )
        .map_err(|_| RecoveryBackupError::PreflightFailed)?;
    validate_configuration_entry(&snapshot.codex, true)?;
    validate_configuration_entry(&snapshot.claude, false)?;
    Ok(account_document)
}

fn validate_configuration_entry(
    entry: &RecoveryFileEntry,
    codex: bool,
) -> Result<(), RecoveryBackupError> {
    if !entry.present {
        return if entry.mode.is_none() && entry.bytes.is_empty() {
            Ok(())
        } else {
            Err(RecoveryBackupError::PreflightFailed)
        };
    }
    let mode = entry.mode.ok_or(RecoveryBackupError::PreflightFailed)?;
    if mode == 0 || mode & 0o022 != 0 {
        return Err(RecoveryBackupError::PreflightFailed);
    }
    if codex {
        let document =
            std::str::from_utf8(&entry.bytes).map_err(|_| RecoveryBackupError::PreflightFailed)?;
        let document = toml_edit::DocumentMut::from_str(document)
            .map_err(|_| RecoveryBackupError::PreflightFailed)?;
        if document
            .get("profile")
            .is_some_and(|profile| !profile.is_none())
        {
            return Err(RecoveryBackupError::PreflightFailed);
        }
    } else {
        let value: serde_json::Value = serde_json::from_slice(&entry.bytes)
            .map_err(|_| RecoveryBackupError::PreflightFailed)?;
        if !value.is_object() {
            return Err(RecoveryBackupError::PreflightFailed);
        }
    }
    Ok(())
}

fn apply_recovery_snapshot(
    home: &MuxviaHome,
    snapshot: &ExtractedRecoverySnapshot,
    hook: Option<&RestoreHook>,
) -> Result<(), RecoveryBackupError> {
    replace_recovery_file(
        ManagedFile::in_configuration_home(home.root(), "state", "subscription-accounts.json")
            .map_err(|_| RecoveryBackupError::RestoreStepFailed {
                stage: "subscription-accounts",
            })?,
        &snapshot.accounts,
    )
    .map_err(|_| RecoveryBackupError::RestoreStepFailed {
        stage: "subscription-accounts",
    })?;
    run_restore_hook(hook, RecoveryRestoreStage::AfterAccounts)?;
    replace_recovery_file(
        ManagedFile::in_configuration_home(home.user_home(), ".codex", "config.toml").map_err(
            |_| RecoveryBackupError::RestoreStepFailed {
                stage: "codex-configuration",
            },
        )?,
        &snapshot.codex,
    )
    .map_err(|_| RecoveryBackupError::RestoreStepFailed {
        stage: "codex-configuration",
    })?;
    run_restore_hook(hook, RecoveryRestoreStage::AfterCodex)?;
    replace_recovery_file(
        ManagedFile::in_configuration_home(home.user_home(), ".claude", "settings.json").map_err(
            |_| RecoveryBackupError::RestoreStepFailed {
                stage: "claude-configuration",
            },
        )?,
        &snapshot.claude,
    )
    .map_err(|_| RecoveryBackupError::RestoreStepFailed {
        stage: "claude-configuration",
    })?;
    run_restore_hook(hook, RecoveryRestoreStage::AfterClaude)?;
    Ok(())
}

fn run_restore_hook(
    hook: Option<&RestoreHook>,
    stage: RecoveryRestoreStage,
) -> Result<(), RecoveryBackupError> {
    hook.map(|hook| hook(stage)).transpose().map_err(|_| {
        RecoveryBackupError::RestoreStepFailed {
            stage: restore_stage_name(stage),
        }
    })?;
    Ok(())
}

fn replace_recovery_file(
    file: ManagedFile,
    desired: &RecoveryFileEntry,
) -> Result<(), RecoveryBackupError> {
    let current = file
        .read()
        .map_err(|_| RecoveryBackupError::RestoreStepFailed {
            stage: "managed-file",
        })?;
    if !current.identity.exists() && !desired.present {
        return Ok(());
    }
    file.replace_with_mode(
        &current.identity,
        &desired.bytes,
        !desired.present,
        desired.mode.unwrap_or(PRIVATE_FILE_MODE),
    )
    .map_err(|_| RecoveryBackupError::RestoreStepFailed {
        stage: "managed-file",
    })
}

fn verify_live_snapshot(
    home: &MuxviaHome,
    snapshot: &ExtractedRecoverySnapshot,
) -> Result<(), RecoveryBackupError> {
    for (stage, file, desired) in [
        (
            "subscription-accounts",
            ManagedFile::in_configuration_home(home.root(), "state", "subscription-accounts.json"),
            &snapshot.accounts,
        ),
        (
            "codex-configuration",
            ManagedFile::in_configuration_home(home.user_home(), ".codex", "config.toml"),
            &snapshot.codex,
        ),
        (
            "claude-configuration",
            ManagedFile::in_configuration_home(home.user_home(), ".claude", "settings.json"),
            &snapshot.claude,
        ),
    ] {
        let current = file
            .map_err(|_| RecoveryBackupError::RestoreStepFailed { stage })?
            .read()
            .map_err(|_| RecoveryBackupError::RestoreStepFailed { stage })?;
        if current.identity.exists() != desired.present
            || current.bytes != desired.bytes
            || current.identity.mode().map(|mode| mode & 0o777) != desired.mode
        {
            return Err(RecoveryBackupError::RestoreStepFailed { stage });
        }
    }
    Ok(())
}

impl CapturedEntry {
    fn from_path(
        kind: RecoveryBackupEntryKind,
        path: PathBuf,
        mode: u32,
    ) -> Result<Self, RecoveryBackupError> {
        let (byte_length, sha256) = hash_file(&path)?;
        Ok(Self {
            manifest: RecoveryBackupEntryManifest {
                kind,
                present: true,
                mode: Some(mode),
                byte_length,
                sha256,
            },
            data: CapturedData::Path(path),
        })
    }

    fn from_contents(kind: RecoveryBackupEntryKind, contents: ManagedFileContents) -> Self {
        let present = contents.identity.exists();
        let mode = contents.identity.mode().map(|mode| mode & 0o777);
        let byte_length = contents.bytes.len() as u64;
        let sha256 = sha256_hex(&contents.bytes);
        Self {
            manifest: RecoveryBackupEntryManifest {
                kind,
                present,
                mode,
                byte_length,
                sha256,
            },
            data: CapturedData::Bytes(contents.bytes),
        }
    }
}

fn managed_file_contents(
    user_home: &Path,
    directory: &str,
    filename: &str,
) -> Result<ManagedFileContents, RecoveryBackupError> {
    ManagedFile::in_configuration_home(user_home, directory, filename)
        .and_then(|file| file.read())
        .map_err(|_| RecoveryBackupError::CreationFailed)
}

fn create_private_empty_file(path: &Path) -> Result<(), RecoveryBackupError> {
    let mut options = OpenOptions::new();
    options
        .create_new(true)
        .read(true)
        .write(true)
        .mode(PRIVATE_FILE_MODE);
    options
        .open(path)
        .map_err(|_| RecoveryBackupError::CreationFailed)?;
    set_private_permissions(path)
}

fn write_container(
    path: &Path,
    manifest: &RecoveryBackupManifest,
    entries: &[CapturedEntry],
) -> Result<(), RecoveryBackupError> {
    let header = serde_json::to_vec(manifest).map_err(|_| RecoveryBackupError::CreationFailed)?;
    let header_length = u32::try_from(header.len())
        .ok()
        .filter(|length| *length as usize <= MAX_HEADER_BYTES)
        .ok_or(RecoveryBackupError::CreationFailed)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true).mode(PRIVATE_FILE_MODE);
    let mut output = options
        .open(path)
        .map_err(|_| RecoveryBackupError::CreationFailed)?;
    set_private_permissions(path)?;
    output
        .write_all(MAGIC)
        .and_then(|_| output.write_all(&header_length.to_be_bytes()))
        .and_then(|_| output.write_all(&header))
        .map_err(|_| RecoveryBackupError::CreationFailed)?;
    for entry in entries {
        match &entry.data {
            CapturedData::Path(path) => {
                let mut input =
                    File::open(path).map_err(|_| RecoveryBackupError::CreationFailed)?;
                let copied = io::copy(&mut input, &mut output)
                    .map_err(|_| RecoveryBackupError::CreationFailed)?;
                if copied != entry.manifest.byte_length {
                    return Err(RecoveryBackupError::SnapshotChanged);
                }
            }
            CapturedData::Bytes(bytes) => output
                .write_all(bytes)
                .map_err(|_| RecoveryBackupError::CreationFailed)?,
        }
    }
    output
        .flush()
        .and_then(|_| output.sync_all())
        .map_err(|_| RecoveryBackupError::CreationFailed)
}

fn inspect_artifact(path: &Path) -> Result<RecoveryBackupInspection, RecoveryBackupError> {
    if !path.is_absolute() || path.as_os_str().len() > MAX_PATH_BYTES {
        return Err(RecoveryBackupError::InvalidPath);
    }
    if fs::symlink_metadata(path)
        .map_err(|_| RecoveryBackupError::InvalidPath)?
        .file_type()
        .is_symlink()
    {
        return Err(RecoveryBackupError::InvalidPath);
    }
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| RecoveryBackupError::InvalidPath)?;
    let metadata = file
        .metadata()
        .map_err(|_| RecoveryBackupError::InvalidPath)?;
    if !metadata.is_file() {
        return Err(RecoveryBackupError::InvalidPath);
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(RecoveryBackupError::UnsafePermissions);
    }
    let artifact_size_bytes = metadata.len();
    let mut magic = vec![0_u8; MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    if magic != MAGIC {
        return Err(RecoveryBackupError::InvalidArtifact);
    }
    let mut header_length = [0_u8; 4];
    file.read_exact(&mut header_length)
        .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    let header_length = u32::from_be_bytes(header_length) as usize;
    if header_length == 0 || header_length > MAX_HEADER_BYTES {
        return Err(RecoveryBackupError::InvalidArtifact);
    }
    let mut header = vec![0_u8; header_length];
    file.read_exact(&mut header)
        .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    let manifest: RecoveryBackupManifest =
        serde_json::from_slice(&header).map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    validate_manifest(&manifest)?;
    let content_length = manifest
        .entries
        .iter()
        .try_fold(0_u64, |total, entry| total.checked_add(entry.byte_length));
    let expected_length = (MAGIC.len() as u64)
        .checked_add(4)
        .and_then(|length| length.checked_add(header_length as u64))
        .and_then(|length| length.checked_add(content_length?))
        .ok_or(RecoveryBackupError::InvalidArtifact)?;
    if expected_length != artifact_size_bytes {
        return Err(RecoveryBackupError::InvalidArtifact);
    }
    for entry in &manifest.entries {
        let mut remaining = entry.byte_length;
        let mut context = Context::new(&SHA256);
        let mut buffer = [0_u8; 64 * 1024];
        while remaining > 0 {
            let length = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
            file.read_exact(&mut buffer[..length])
                .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
            context.update(&buffer[..length]);
            remaining -= length as u64;
        }
        if hex(context.finish().as_ref()) != entry.sha256 {
            return Err(RecoveryBackupError::InvalidArtifact);
        }
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
    let artifact_sha256 = hash_reader(&mut file)?;
    let compatibility = match manifest.database_schema_version {
        SCHEMA_VERSION => RecoveryBackupCompatibility::Compatible,
        1..SCHEMA_VERSION => RecoveryBackupCompatibility::MigrationRequired,
        _ => RecoveryBackupCompatibility::UnsupportedDatabaseSchema,
    };
    Ok(RecoveryBackupInspection {
        snapshot_id: manifest.snapshot_id,
        created_at_unix_seconds: manifest.created_at_unix_seconds,
        created_by_release: manifest.created_by_release,
        format_version: manifest.format_version,
        database_schema_version: manifest.database_schema_version,
        artifact_size_bytes,
        artifact_sha256,
        sensitive: true,
        compatibility,
        entries: manifest
            .entries
            .into_iter()
            .map(|entry| RecoveryBackupEntrySummary {
                kind: entry.kind,
                present: entry.present,
                mode: entry.mode,
                byte_length: entry.byte_length,
            })
            .collect(),
    })
}

fn validate_manifest(manifest: &RecoveryBackupManifest) -> Result<(), RecoveryBackupError> {
    if manifest.format != FORMAT
        || manifest.format_version != FORMAT_VERSION
        || !manifest.sensitive
        || manifest.created_at_unix_seconds == 0
        || manifest.created_by_release.is_empty()
        || manifest.created_by_release.len() > MAX_RELEASE_BYTES
        || !manifest
            .created_by_release
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
        || manifest.database_schema_version == 0
        || manifest.entries.len() != ENTRY_KINDS.len()
    {
        return Err(RecoveryBackupError::InvalidArtifact);
    }
    for (entry, expected_kind) in manifest.entries.iter().zip(ENTRY_KINDS) {
        if entry.kind != expected_kind
            || entry.sha256.len() != 64
            || !entry.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || entry.mode.is_some_and(|mode| mode > 0o777)
            || (!entry.present
                && (entry.byte_length != 0
                    || entry.mode.is_some()
                    || entry.sha256 != sha256_hex(&[])))
        {
            return Err(RecoveryBackupError::InvalidArtifact);
        }
    }
    let database = &manifest.entries[0];
    if !database.present || database.mode != Some(PRIVATE_FILE_MODE) || database.byte_length == 0 {
        return Err(RecoveryBackupError::InvalidArtifact);
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<(u64, String), RecoveryBackupError> {
    let mut file = File::open(path).map_err(|_| RecoveryBackupError::CreationFailed)?;
    let length = file
        .metadata()
        .map_err(|_| RecoveryBackupError::CreationFailed)?
        .len();
    let sha256 = hash_reader(&mut file)?;
    Ok((length, sha256))
}

fn hash_reader(reader: &mut impl Read) -> Result<String, RecoveryBackupError> {
    let mut context = Context::new(&SHA256);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| RecoveryBackupError::InvalidArtifact)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(hex(context.finish().as_ref()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(digest(&SHA256, bytes).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn set_private_permissions(path: &Path) -> Result<(), RecoveryBackupError> {
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
        .map_err(|_| RecoveryBackupError::CreationFailed)
}

fn sync_directory(path: &Path) -> Result<(), RecoveryBackupError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| RecoveryBackupError::CreationFailed)
}

struct PendingFile {
    path: Option<PathBuf>,
}

impl PendingFile {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(&path);
            if let Some(parent) = path.parent() {
                let _ = sync_directory(parent);
            }
        }
    }
}

struct PendingDirectory {
    path: PathBuf,
}

impl PendingDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for PendingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        if let Some(parent) = self.path.parent() {
            let _ = sync_directory(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        io::{self, Read},
        os::unix::fs::{PermissionsExt, symlink},
        path::{Path, PathBuf},
        sync::Arc,
    };

    use tempfile::TempDir;

    use super::{
        MAGIC, RecoveryBackupError, RecoveryBackupService, RecoveryRestoreStage, inspect_artifact,
    };
    use crate::{
        codex::CommandCodexProbe,
        control::protocol::{ClaudeHostManagedState, ClaudePreflightContext, ClaudeSelectorState},
        home::MuxviaHome,
        model::ReqwestUpstream,
        service::{activate::ActivationService, reconcile::ReconciliationService},
        state::StateStore,
        subscription::{
            DeviceAuthorizationManager, ReqwestDeviceAuthorizationAuthority,
            SubscriptionAccountCoordinator, SubscriptionAccountStore,
        },
    };

    async fn fixture() -> (TempDir, MuxviaHome, Arc<StateStore>, RecoveryBackupService) {
        let root = tempfile::tempdir().expect("temporary root");
        let user_home = root.path().join("home");
        fs::create_dir(&user_home).expect("user home");
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.expect("state store"));
        let activation = Arc::new(ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(CommandCodexProbe),
            PathBuf::from("/usr/bin/false"),
            Arc::new(ReqwestUpstream::new().expect("upstream")),
        ));
        let reconciliation = Arc::new(ReconciliationService::from_runtime(
            Arc::clone(&store),
            activation.reconciliation_runtime(),
        ));
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("accounts"));
        let account_coordinator = Arc::new(SubscriptionAccountCoordinator::new(
            Arc::clone(&store),
            Arc::clone(&accounts),
        ));
        let device_authorization = Arc::new(DeviceAuthorizationManager::new(
            Arc::clone(&accounts),
            Arc::clone(&account_coordinator),
            Arc::new(ReqwestDeviceAuthorizationAuthority::new().expect("device authority")),
        ));
        let service = RecoveryBackupService::new(
            Arc::clone(&store),
            home.clone(),
            reconciliation,
            activation,
            accounts,
            account_coordinator,
            device_authorization,
            "0.1.0".to_owned(),
        )
        .expect("Recovery Backup service");
        (root, home, store, service)
    }

    fn write_private(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("file parent")).expect("create file parent");
        fs::write(path, bytes).expect("write private fixture");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private fixture mode");
    }

    fn claude_context(home: &MuxviaHome) -> ClaudePreflightContext {
        ClaudePreflightContext {
            claude_config_dir: None,
            selector_state: ClaudeSelectorState::Unset,
            blocking_selector: None,
            host_managed_state: ClaudeHostManagedState::Unmanaged,
            cwd: home.user_home().display().to_string(),
        }
    }

    async fn install_sensitive_state(home: &MuxviaHome) {
        write_private(
            &home.subscription_accounts_path(),
            br#"{"version":1,"accounts":{"account-primary":{"account_id":"account-primary","email":"operator@example.test","refresh_token":"RECOVERY_REFRESH_SECRET_17001","authenticated_at":1700000000,"state":"authorized"}},"default_account_id":"account-primary"}"#,
        );
        write_private(
            &home.user_home().join(".codex/config.toml"),
            b"model = \"backup-model\"\n# RECOVERY_CODEX_SECRET_17002\n",
        );
        write_private(
            &home.user_home().join(".claude/settings.json"),
            br#"{"env":{"ANTHROPIC_AUTH_TOKEN":"RECOVERY_CLAUDE_SECRET_17003"}}"#,
        );
        let database = tokio_rusqlite::Connection::open(home.database_path())
            .await
            .expect("seed database");
        database
            .call(|connection| {
                connection.execute(
                    "INSERT INTO metadata(key, value) VALUES ('recovery-secret-test', 'RECOVERY_DATABASE_SECRET_17004')",
                    [],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .expect("seed private database state");
    }

    async fn replace_sensitive_state(home: &MuxviaHome) {
        write_private(
            &home.subscription_accounts_path(),
            br#"{"version":1,"accounts":{"account-replacement":{"account_id":"account-replacement","email":"replacement@example.test","refresh_token":"RECOVERY_REFRESH_REPLACEMENT_18001","authenticated_at":1800000000,"state":"authorized"}},"default_account_id":"account-replacement"}"#,
        );
        write_private(
            &home.user_home().join(".codex/config.toml"),
            b"model = \"replacement-model\"\n# RECOVERY_CODEX_REPLACEMENT_18002\n",
        );
        write_private(
            &home.user_home().join(".claude/settings.json"),
            br#"{"env":{"ANTHROPIC_AUTH_TOKEN":"RECOVERY_CLAUDE_REPLACEMENT_18003"}}"#,
        );
        let database = tokio_rusqlite::Connection::open(home.database_path())
            .await
            .expect("open replacement database");
        database
            .call(|connection| {
                connection.execute(
                    "UPDATE metadata SET value = 'RECOVERY_DATABASE_REPLACEMENT_18004' WHERE key = 'recovery-secret-test'",
                    [],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .expect("replace private database state");
    }

    fn container_entries(path: &Path) -> Vec<Vec<u8>> {
        let mut file = File::open(path).expect("container");
        let mut magic = vec![0_u8; MAGIC.len()];
        file.read_exact(&mut magic).expect("magic");
        assert_eq!(magic, MAGIC);
        let mut length = [0_u8; 4];
        file.read_exact(&mut length).expect("manifest length");
        let mut header = vec![0_u8; u32::from_be_bytes(length) as usize];
        file.read_exact(&mut header).expect("manifest");
        let manifest: super::RecoveryBackupManifest =
            serde_json::from_slice(&header).expect("manifest JSON");
        manifest
            .entries
            .into_iter()
            .map(|entry| {
                let mut bytes = vec![0_u8; entry.byte_length as usize];
                file.read_exact(&mut bytes).expect("entry");
                bytes
            })
            .collect()
    }

    #[tokio::test]
    async fn creation_captures_one_private_complete_snapshot_and_inspection_is_content_free() {
        let (_root, home, _store, service) = fixture().await;
        install_sensitive_state(&home).await;

        let created = service.create().await.expect("create Recovery Backup");
        assert_eq!(created.path.parent(), Some(home.backups_dir().as_path()));
        assert!(
            created
                .path
                .extension()
                .is_some_and(|value| value == "muxvia-recovery")
        );
        assert_eq!(
            fs::metadata(&created.path)
                .expect("backup metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(created.inspection.sensitive);
        assert_eq!(created.inspection.entries.len(), 4);
        let rendered = serde_json::to_string(&created.inspection).expect("safe inspection JSON");
        for secret in [
            "RECOVERY_REFRESH_SECRET_17001",
            "RECOVERY_CODEX_SECRET_17002",
            "RECOVERY_CLAUDE_SECRET_17003",
            "RECOVERY_DATABASE_SECRET_17004",
        ] {
            assert!(!rendered.contains(secret), "inspection exposed {secret}");
        }

        let entries = container_entries(&created.path);
        assert!(
            entries[0]
                .windows("RECOVERY_DATABASE_SECRET_17004".len())
                .any(|window| window == b"RECOVERY_DATABASE_SECRET_17004")
        );
        assert!(String::from_utf8_lossy(&entries[1]).contains("RECOVERY_REFRESH_SECRET_17001"));
        assert!(String::from_utf8_lossy(&entries[2]).contains("RECOVERY_CODEX_SECRET_17002"));
        assert!(String::from_utf8_lossy(&entries[3]).contains("RECOVERY_CLAUDE_SECRET_17003"));
        assert_eq!(service.inspect(&created.path).unwrap(), created.inspection);
    }

    #[tokio::test]
    async fn restore_replaces_the_complete_snapshot_and_retains_a_pre_restore_backup() {
        let (_root, home, _store, service) = fixture().await;
        install_sensitive_state(&home).await;
        let selected = service.create().await.expect("selected Recovery Backup");
        replace_sensitive_state(&home).await;

        let restored = service
            .restore(&selected.path, Some(claude_context(&home)))
            .await
            .expect("restore Recovery Backup");

        assert_eq!(
            restored.restored_snapshot_id,
            selected.inspection.snapshot_id
        );
        assert_ne!(
            restored.pre_restore_snapshot_id,
            selected.inspection.snapshot_id
        );
        assert!(restored.pre_restore_backup_path.exists());
        let restored_entries = container_entries(&service.create().await.unwrap().path);
        for (entry, original) in restored_entries
            .iter()
            .zip(container_entries(&selected.path))
        {
            assert_eq!(entry, &original);
        }
        let pre_restore_entries = container_entries(&restored.pre_restore_backup_path);
        for secret in [
            "RECOVERY_REFRESH_REPLACEMENT_18001",
            "RECOVERY_CODEX_REPLACEMENT_18002",
            "RECOVERY_CLAUDE_REPLACEMENT_18003",
            "RECOVERY_DATABASE_REPLACEMENT_18004",
        ] {
            assert!(
                pre_restore_entries
                    .iter()
                    .any(|entry| String::from_utf8_lossy(entry).contains(secret)),
                "pre-restore snapshot omitted {secret}"
            );
        }
    }

    #[tokio::test]
    async fn every_restore_boundary_exactly_recovers_the_prior_snapshot() {
        for injected_stage in [
            RecoveryRestoreStage::AfterDatabase,
            RecoveryRestoreStage::AfterAccounts,
            RecoveryRestoreStage::AfterCodex,
            RecoveryRestoreStage::AfterClaude,
            RecoveryRestoreStage::BeforeRuntimeResume,
            RecoveryRestoreStage::Verification,
        ] {
            let (_root, home, _store, service) = fixture().await;
            install_sensitive_state(&home).await;
            let selected = service.create().await.expect("selected Recovery Backup");
            replace_sensitive_state(&home).await;
            let service = service.with_restore_hook(Arc::new(move |stage| {
                if stage == injected_stage {
                    Err(io::Error::other("injected restore boundary failure"))
                } else {
                    Ok(())
                }
            }));

            assert!(matches!(
                service
                    .restore(&selected.path, Some(claude_context(&home)))
                    .await,
                Err(RecoveryBackupError::RestoreFailed { .. })
            ));
            let current = container_entries(&service.create().await.unwrap().path);
            for secret in [
                "RECOVERY_REFRESH_REPLACEMENT_18001",
                "RECOVERY_CODEX_REPLACEMENT_18002",
                "RECOVERY_CLAUDE_REPLACEMENT_18003",
                "RECOVERY_DATABASE_REPLACEMENT_18004",
            ] {
                assert!(
                    current
                        .iter()
                        .any(|entry| String::from_utf8_lossy(entry).contains(secret)),
                    "{injected_stage:?} rollback omitted {secret}"
                );
            }
        }
    }

    #[tokio::test]
    async fn rollback_failure_enters_recovery_required_and_retains_actionable_backup() {
        let (_root, home, store, service) = fixture().await;
        install_sensitive_state(&home).await;
        let selected = service.create().await.expect("selected Recovery Backup");
        replace_sensitive_state(&home).await;
        let service = service.with_restore_hook(Arc::new(|stage| {
            if matches!(
                stage,
                RecoveryRestoreStage::AfterAccounts | RecoveryRestoreStage::BeforeRollback
            ) {
                Err(io::Error::other("injected restore and rollback failure"))
            } else {
                Ok(())
            }
        }));

        let recovery_path = match service
            .restore(&selected.path, Some(claude_context(&home)))
            .await
        {
            Err(RecoveryBackupError::RecoveryRequired {
                pre_restore_backup_path,
                ..
            }) => pre_restore_backup_path,
            _ => panic!("rollback failure did not enter Recovery Required"),
        };
        assert!(recovery_path.exists());
        assert!(service.inspect(&recovery_path).is_ok());
        for target in [
            crate::control::protocol::Target::Codex,
            crate::control::protocol::Target::Claude,
        ] {
            let view = store.target_view_for(target).await.expect("Recovery View");
            assert_eq!(view.recovery.state, "recovery-required");
        }
        let accounts = SubscriptionAccountStore::open(&home)
            .expect("Subscription Account store")
            .read()
            .expect("Subscription Account file");
        let account_view = store
            .subscription_account_catalog(accounts.document)
            .await
            .expect("Subscription Account Recovery View");
        assert_eq!(
            account_view.recovery.state,
            crate::control::protocol::SubscriptionAccountRecoveryState::RecoveryRequired
        );
    }

    #[tokio::test]
    async fn restore_preflight_rejects_a_profile_shadow_without_creating_a_recovery_point() {
        let (_root, home, _store, service) = fixture().await;
        install_sensitive_state(&home).await;
        write_private(
            &home.user_home().join(".codex/config.toml"),
            b"model = \"backup-model\"\nprofile = \"shadow-profile\"\n",
        );
        let selected = service.create().await.expect("selected Recovery Backup");
        replace_sensitive_state(&home).await;

        assert!(matches!(
            service
                .restore(&selected.path, Some(claude_context(&home)))
                .await,
            Err(RecoveryBackupError::PreflightFailed)
        ));
        let backups = fs::read_dir(home.backups_dir())
            .expect("backup directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("backup entries");
        assert_eq!(backups.len(), 1, "preflight created a recovery point");
        let current = container_entries(&service.create().await.unwrap().path);
        assert!(current.iter().any(|entry| {
            String::from_utf8_lossy(entry).contains("RECOVERY_DATABASE_REPLACEMENT_18004")
        }));
    }

    #[tokio::test]
    async fn interrupted_creation_leaves_no_backup_or_live_state_mutation() {
        let (_root, home, store, service) = fixture().await;
        install_sensitive_state(&home).await;
        let before = store
            .target_view_for(crate::control::protocol::Target::Codex)
            .await
            .expect("before view");
        let service = service
            .with_before_install_hook(Arc::new(|_| Err(io::Error::other("injected interruption"))));

        assert!(matches!(
            service.create().await,
            Err(RecoveryBackupError::CreationFailed)
        ));
        let after = store
            .target_view_for(crate::control::protocol::Target::Codex)
            .await
            .expect("after view");
        assert_eq!(before, after);
        let leftovers = fs::read_dir(home.backups_dir())
            .expect("backup directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("backup entries");
        assert!(
            leftovers.is_empty(),
            "interrupted creation retained staging files"
        );
    }

    #[tokio::test]
    async fn inspection_rejects_corruption_unsafe_permissions_and_nonbackup_files() {
        let (_root, home, _store, service) = fixture().await;
        let created = service.create().await.expect("create Recovery Backup");
        let mut bytes = fs::read(&created.path).expect("backup bytes");
        let last = bytes.last_mut().expect("nonempty backup");
        *last ^= 0x55;
        let corrupt = home.backups_dir().join("corrupt.muxvia-recovery");
        write_private(&corrupt, &bytes);
        assert_eq!(
            inspect_artifact(&corrupt),
            Err(RecoveryBackupError::InvalidArtifact)
        );

        let original = fs::read(&created.path).expect("original backup");
        let current = b"\"databaseSchemaVersion\":17";
        let newer = b"\"databaseSchemaVersion\":18";
        let offset = original
            .windows(current.len())
            .position(|window| window == current)
            .expect("schema version in manifest");
        let mut incompatible = original.clone();
        incompatible[offset..offset + newer.len()].copy_from_slice(newer);
        let incompatible_path = home.backups_dir().join("future.muxvia-recovery");
        write_private(&incompatible_path, &incompatible);
        assert_eq!(
            inspect_artifact(&incompatible_path)
                .expect("inspect future backup")
                .compatibility,
            crate::control::protocol::RecoveryBackupCompatibility::UnsupportedDatabaseSchema
        );
        let symlink_path = home.backups_dir().join("linked.muxvia-recovery");
        symlink(&incompatible_path, &symlink_path).expect("backup symlink");
        assert_eq!(
            inspect_artifact(&symlink_path),
            Err(RecoveryBackupError::InvalidPath)
        );

        fs::set_permissions(&created.path, fs::Permissions::from_mode(0o644)).expect("unsafe mode");
        assert_eq!(
            inspect_artifact(&created.path),
            Err(RecoveryBackupError::UnsafePermissions)
        );
        assert_eq!(
            inspect_artifact(home.database_path()),
            Err(RecoveryBackupError::InvalidArtifact)
        );
        assert_eq!(
            inspect_artifact(Path::new("relative.muxvia-recovery")),
            Err(RecoveryBackupError::InvalidPath)
        );
    }
}
