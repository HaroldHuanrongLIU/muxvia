use std::{
    cell::Cell,
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

pub(crate) type PreRenameHook = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
pub(crate) type ExchangeHook = Arc<dyn Fn(&Path, &Path) -> io::Result<bool> + Send + Sync>;
type DirectorySyncHook = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;
type RollbackHook = Arc<dyn Fn() -> bool + Send + Sync>;

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

impl FileIdentity {
    pub(crate) fn exists(&self) -> bool {
        self.exists
    }

    fn same_inode(&self, other: &Self) -> bool {
        self.exists && other.exists && self.device == other.device && self.inode == other.inode
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedFileContents {
    pub(crate) identity: FileIdentity,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedFileError {
    WriteFailed,
    RecoveryRequired,
}

pub(crate) struct ManagedFile {
    path: PathBuf,
    pre_rename_hook: Option<PreRenameHook>,
    exchange_hook: Option<ExchangeHook>,
    directory_sync_hook: Option<DirectorySyncHook>,
    rollback_hook: Option<RollbackHook>,
}

impl ManagedFile {
    pub(crate) fn in_configuration_home(
        user_home: &Path,
        directory_name: &str,
        file_name: &str,
    ) -> Result<Self, ManagedFileError> {
        Self::build(user_home, directory_name, file_name, None, None, None, None)
    }

    pub(crate) fn with_pre_rename_hook(
        user_home: &Path,
        directory_name: &str,
        file_name: &str,
        hook: PreRenameHook,
    ) -> Result<Self, ManagedFileError> {
        Self::build(
            user_home,
            directory_name,
            file_name,
            Some(hook),
            None,
            None,
            None,
        )
    }

    pub(crate) fn with_exchange_hook(
        user_home: &Path,
        directory_name: &str,
        file_name: &str,
        hook: ExchangeHook,
    ) -> Result<Self, ManagedFileError> {
        Self::build(
            user_home,
            directory_name,
            file_name,
            None,
            Some(hook),
            None,
            None,
        )
    }

    #[cfg(test)]
    fn with_fault_hooks(
        user_home: &Path,
        directory_name: &str,
        file_name: &str,
        directory_sync_hook: DirectorySyncHook,
        fail_rollback: bool,
    ) -> Result<Self, ManagedFileError> {
        Self::build(
            user_home,
            directory_name,
            file_name,
            None,
            None,
            Some(directory_sync_hook),
            Some(Arc::new(move || fail_rollback)),
        )
    }

    fn build(
        user_home: &Path,
        directory_name: &str,
        file_name: &str,
        pre_rename_hook: Option<PreRenameHook>,
        exchange_hook: Option<ExchangeHook>,
        directory_sync_hook: Option<DirectorySyncHook>,
        rollback_hook: Option<RollbackHook>,
    ) -> Result<Self, ManagedFileError> {
        let configured_home = user_home.join(directory_name);
        let config_home = match fs::symlink_metadata(&configured_home) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                fs::canonicalize(&configured_home).map_err(|_| ManagedFileError::WriteFailed)?
            }
            Ok(metadata) if metadata.is_dir() => configured_home,
            Ok(_) => return Err(ManagedFileError::WriteFailed),
            Err(error) if error.kind() == io::ErrorKind::NotFound => configured_home,
            Err(_) => return Err(ManagedFileError::WriteFailed),
        };
        Ok(Self {
            path: config_home.join(file_name),
            pre_rename_hook,
            exchange_hook,
            directory_sync_hook,
            rollback_hook,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn read(&self) -> Result<ManagedFileContents, ManagedFileError> {
        let parent = self.path.parent().ok_or(ManagedFileError::WriteFailed)?;
        let directory = match rustix::fs::openat(
            rustix::fs::CWD,
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => Some(directory),
            Err(error) if error == rustix::io::Errno::NOENT => None,
            Err(_) => return Err(ManagedFileError::WriteFailed),
        };
        let (identity, bytes) = if let Some(directory) = directory {
            let parent_identity =
                directory_identity(&directory).map_err(|_| ManagedFileError::WriteFailed)?;
            if !path_matches_directory(parent, parent_identity) {
                return Err(ManagedFileError::WriteFailed);
            }
            let target_name = self.path.file_name().ok_or(ManagedFileError::WriteFailed)?;
            let identity = file_identity_at(&directory, target_name)?;
            let bytes = if identity.exists {
                let file = rustix::fs::openat(
                    &directory,
                    target_name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| ManagedFileError::WriteFailed)?;
                let mut file = File::from(file);
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|_| ManagedFileError::WriteFailed)?;
                if file_identity_from_stat(
                    rustix::fs::fstat(&file).map_err(|_| ManagedFileError::WriteFailed)?,
                ) != identity
                {
                    return Err(ManagedFileError::WriteFailed);
                }
                bytes
            } else {
                Vec::new()
            };
            (identity, bytes)
        } else {
            (missing_file_identity(), Vec::new())
        };
        Ok(ManagedFileContents { identity, bytes })
    }

    pub(crate) fn replace(
        &self,
        expected: &FileIdentity,
        bytes: &[u8],
        remove_file: bool,
    ) -> Result<(), ManagedFileError> {
        let parent = self.path.parent().ok_or(ManagedFileError::WriteFailed)?;
        create_private_parent(parent).map_err(|_| ManagedFileError::WriteFailed)?;
        let directory = rustix::fs::openat(
            rustix::fs::CWD,
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| ManagedFileError::WriteFailed)?;
        let directory_identity =
            directory_identity(&directory).map_err(|_| ManagedFileError::WriteFailed)?;
        if !path_matches_directory(parent, directory_identity) {
            return Err(ManagedFileError::WriteFailed);
        }
        let target_name = self.path.file_name().ok_or(ManagedFileError::WriteFailed)?;
        if file_identity_at(&directory, target_name)? != *expected {
            return Err(ManagedFileError::WriteFailed);
        }
        let temporary_name = format!(".muxvia-{}.tmp", uuid::Uuid::new_v4());
        let temporary_fd = rustix::fs::openat(
            &directory,
            temporary_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(expected.mode.unwrap_or(0o600) as _),
        )
        .map_err(|_| ManagedFileError::WriteFailed)?;
        let mut temporary = File::from(temporary_fd);
        rustix::fs::fchmod(
            &temporary,
            Mode::from_raw_mode(expected.mode.unwrap_or(0o600) as _),
        )
        .map_err(|_| ManagedFileError::WriteFailed)?;
        let retain_temporary = Cell::new(false);
        let result = (|| {
            temporary
                .write_all(bytes)
                .and_then(|_| temporary.flush())
                .and_then(|_| temporary.sync_all())
                .map_err(|_| ManagedFileError::WriteFailed)?;
            let written_identity = file_identity_from_stat(
                rustix::fs::fstat(&temporary).map_err(|_| ManagedFileError::WriteFailed)?,
            );
            if let Some(hook) = &self.pre_rename_hook {
                hook(&parent.join(&temporary_name)).map_err(|_| ManagedFileError::WriteFailed)?;
            }
            if !path_matches_directory(parent, directory_identity)
                || file_identity_at(&directory, temporary_name.as_str())? != written_identity
                || file_identity_at(&directory, target_name)? != *expected
            {
                return Err(ManagedFileError::WriteFailed);
            }
            let injected_rollback_failure = self.rollback_hook.as_ref().is_some_and(|hook| hook());
            if expected.exists {
                rustix::fs::renameat_with(
                    &directory,
                    temporary_name.as_str(),
                    &directory,
                    target_name,
                    RenameFlags::EXCHANGE,
                )
                .map_err(|_| ManagedFileError::WriteFailed)?;
                retain_temporary.set(true);
                let inject_rollback_failure = self
                    .exchange_hook
                    .as_ref()
                    .map(|hook| hook(&parent.join(&temporary_name), &self.path))
                    .transpose()
                    .map_err(|_| ManagedFileError::RecoveryRequired)?
                    .unwrap_or(false)
                    || injected_rollback_failure;
                let displaced_identity = file_identity_at(&directory, temporary_name.as_str())
                    .map_err(|_| ManagedFileError::RecoveryRequired)?;
                let installed_identity = file_identity_at(&directory, target_name)
                    .map_err(|_| ManagedFileError::RecoveryRequired)?;
                let displaced_matches = displaced_identity == *expected;
                let target_matches = installed_identity == written_identity;
                if !displaced_matches || !target_matches {
                    let rolled_back = displaced_identity.same_inode(expected)
                        && !inject_rollback_failure
                        && rustix::fs::renameat_with(
                            &directory,
                            temporary_name.as_str(),
                            &directory,
                            target_name,
                            RenameFlags::EXCHANGE,
                        )
                        .is_ok()
                        && file_identity_at(&directory, target_name)
                            .is_ok_and(|identity| identity == displaced_identity)
                        && file_identity_at(&directory, temporary_name.as_str())
                            .is_ok_and(|identity| identity == installed_identity)
                        && self.sync_directory(&directory).is_ok();
                    if !rolled_back {
                        return Err(ManagedFileError::RecoveryRequired);
                    }
                    retain_temporary.set(false);
                    return Err(ManagedFileError::WriteFailed);
                }
                if remove_file {
                    rustix::fs::unlinkat(&directory, target_name, AtFlags::empty())
                        .map_err(|_| ManagedFileError::RecoveryRequired)?;
                }
                if self.sync_directory(&directory).is_err() {
                    let rolled_back = if inject_rollback_failure {
                        false
                    } else if remove_file {
                        rustix::fs::renameat_with(
                            &directory,
                            temporary_name.as_str(),
                            &directory,
                            target_name,
                            RenameFlags::NOREPLACE,
                        )
                        .is_ok()
                            && file_identity_at(&directory, target_name)
                                .is_ok_and(|identity| identity == *expected)
                            && self.sync_directory(&directory).is_ok()
                    } else {
                        rustix::fs::renameat_with(
                            &directory,
                            temporary_name.as_str(),
                            &directory,
                            target_name,
                            RenameFlags::EXCHANGE,
                        )
                        .is_ok()
                            && file_identity_at(&directory, target_name)
                                .is_ok_and(|identity| identity == *expected)
                            && file_identity_at(&directory, temporary_name.as_str())
                                .is_ok_and(|identity| identity == written_identity)
                            && self.sync_directory(&directory).is_ok()
                    };
                    if !rolled_back {
                        return Err(ManagedFileError::RecoveryRequired);
                    }
                    retain_temporary.set(false);
                    return Err(ManagedFileError::WriteFailed);
                }
                retain_temporary.set(false);
            } else {
                rustix::fs::renameat_with(
                    &directory,
                    temporary_name.as_str(),
                    &directory,
                    target_name,
                    RenameFlags::NOREPLACE,
                )
                .map_err(|_| ManagedFileError::WriteFailed)?;
                retain_temporary.set(true);
                if file_identity_at(&directory, target_name)
                    .map_err(|_| ManagedFileError::RecoveryRequired)?
                    != written_identity
                {
                    return Err(ManagedFileError::RecoveryRequired);
                }
                if self.sync_directory(&directory).is_err() {
                    let rolled_back = !injected_rollback_failure
                        && rustix::fs::renameat_with(
                            &directory,
                            target_name,
                            &directory,
                            temporary_name.as_str(),
                            RenameFlags::NOREPLACE,
                        )
                        .is_ok()
                        && !file_identity_at(&directory, target_name)
                            .map_err(|_| ManagedFileError::RecoveryRequired)?
                            .exists()
                        && file_identity_at(&directory, temporary_name.as_str())
                            .map_err(|_| ManagedFileError::RecoveryRequired)?
                            == written_identity;
                    if !rolled_back || self.sync_directory(&directory).is_err() {
                        return Err(ManagedFileError::RecoveryRequired);
                    }
                    retain_temporary.set(false);
                    return Err(ManagedFileError::WriteFailed);
                }
                retain_temporary.set(false);
            }
            Ok(())
        })();
        if !retain_temporary.get() {
            let _ = rustix::fs::unlinkat(&directory, temporary_name.as_str(), AtFlags::empty());
        }
        result
    }

    fn sync_directory(&self, directory: &impl AsFd) -> Result<(), ManagedFileError> {
        if let Some(hook) = &self.directory_sync_hook {
            hook().map_err(|_| ManagedFileError::WriteFailed)?;
        }
        rustix::fs::fsync(directory).map_err(|_| ManagedFileError::WriteFailed)
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
) -> Result<FileIdentity, ManagedFileError> {
    let stat = match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(missing_file_identity()),
        Err(_) => return Err(ManagedFileError::WriteFailed),
    };
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(ManagedFileError::WriteFailed);
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
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sync_failing_first(times: usize) -> (DirectorySyncHook, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_hook = Arc::clone(&calls);
        let hook = Arc::new(move || {
            let call = calls_for_hook.fetch_add(1, Ordering::SeqCst);
            if call < times {
                Err(io::Error::other("injected directory sync failure"))
            } else {
                Ok(())
            }
        });
        (hook, calls)
    }

    fn temporary_artifacts(managed: &ManagedFile) -> Vec<PathBuf> {
        fs::read_dir(managed.path().parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".muxvia-"))
            .map(|entry| entry.path())
            .collect()
    }

    #[test]
    fn substituted_temporary_path_is_not_committed_by_shared_seam() {
        let home = tempfile::TempDir::new().unwrap();
        let plain = ManagedFile::in_configuration_home(home.path(), ".target", "settings").unwrap();
        fs::create_dir_all(plain.path().parent().unwrap()).unwrap();
        fs::write(plain.path(), b"operator-original").unwrap();
        let before = plain.read().unwrap().identity;
        let managed = ManagedFile::with_pre_rename_hook(
            home.path(),
            ".target",
            "settings",
            Arc::new(|temporary| {
                fs::rename(temporary, temporary.with_extension("parked"))?;
                fs::write(temporary, b"attacker-substitute")
            }),
        )
        .unwrap();

        let error = managed
            .replace(&before, b"muxvia-desired", false)
            .unwrap_err();

        assert_eq!(error, ManagedFileError::WriteFailed);
        assert_eq!(fs::read(managed.path()).unwrap(), b"operator-original");
    }

    #[test]
    fn existing_target_directory_sync_failure_rolls_back_and_cleans_artifact() {
        let home = tempfile::TempDir::new().unwrap();
        let plain = ManagedFile::in_configuration_home(home.path(), ".target", "settings").unwrap();
        fs::create_dir_all(plain.path().parent().unwrap()).unwrap();
        fs::write(plain.path(), b"operator-original").unwrap();
        let before = plain.read().unwrap().identity;
        let (sync_hook, calls) = sync_failing_first(1);
        let managed =
            ManagedFile::with_fault_hooks(home.path(), ".target", "settings", sync_hook, false)
                .unwrap();

        let error = managed
            .replace(&before, b"muxvia-desired", false)
            .unwrap_err();

        assert_eq!(error, ManagedFileError::WriteFailed);
        assert_eq!(fs::read(managed.path()).unwrap(), b"operator-original");
        assert!(temporary_artifacts(&managed).is_empty());
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn absent_target_directory_sync_failure_restores_absence_and_cleans_artifact() {
        let home = tempfile::TempDir::new().unwrap();
        let plain = ManagedFile::in_configuration_home(home.path(), ".target", "settings").unwrap();
        let before = plain.read().unwrap().identity;
        let (sync_hook, calls) = sync_failing_first(1);
        let managed =
            ManagedFile::with_fault_hooks(home.path(), ".target", "settings", sync_hook, false)
                .unwrap();

        let error = managed
            .replace(&before, b"muxvia-desired", false)
            .unwrap_err();

        assert_eq!(error, ManagedFileError::WriteFailed);
        assert!(!managed.path().exists());
        assert!(temporary_artifacts(&managed).is_empty());
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn existing_target_rollback_sync_failure_retains_written_artifact() {
        let home = tempfile::TempDir::new().unwrap();
        let plain = ManagedFile::in_configuration_home(home.path(), ".target", "settings").unwrap();
        fs::create_dir_all(plain.path().parent().unwrap()).unwrap();
        fs::write(plain.path(), b"operator-original").unwrap();
        let before = plain.read().unwrap().identity;
        let (sync_hook, _) = sync_failing_first(2);
        let managed =
            ManagedFile::with_fault_hooks(home.path(), ".target", "settings", sync_hook, false)
                .unwrap();

        let error = managed
            .replace(&before, b"muxvia-desired", false)
            .unwrap_err();

        assert_eq!(error, ManagedFileError::RecoveryRequired);
        assert_eq!(fs::read(managed.path()).unwrap(), b"operator-original");
        assert!(
            temporary_artifacts(&managed)
                .iter()
                .any(|path| fs::read(path).is_ok_and(|bytes| bytes == b"muxvia-desired"))
        );
    }

    #[test]
    fn absent_target_rollback_sync_failure_retains_written_artifact() {
        let home = tempfile::TempDir::new().unwrap();
        let plain = ManagedFile::in_configuration_home(home.path(), ".target", "settings").unwrap();
        let before = plain.read().unwrap().identity;
        let (sync_hook, _) = sync_failing_first(2);
        let managed =
            ManagedFile::with_fault_hooks(home.path(), ".target", "settings", sync_hook, false)
                .unwrap();

        let error = managed
            .replace(&before, b"routing-secret-sentinel", false)
            .unwrap_err();

        assert_eq!(error, ManagedFileError::RecoveryRequired);
        assert!(!managed.path().exists());
        assert!(
            temporary_artifacts(&managed)
                .iter()
                .any(|path| fs::read(path).is_ok_and(|bytes| bytes == b"routing-secret-sentinel"))
        );
        assert!(!format!("{error:?}").contains("routing-secret-sentinel"));
    }

    #[test]
    fn rollback_operation_failure_retains_displaced_original() {
        let home = tempfile::TempDir::new().unwrap();
        let plain = ManagedFile::in_configuration_home(home.path(), ".target", "settings").unwrap();
        fs::create_dir_all(plain.path().parent().unwrap()).unwrap();
        fs::write(plain.path(), b"operator-original").unwrap();
        let before = plain.read().unwrap().identity;
        let (sync_hook, _) = sync_failing_first(1);
        let managed =
            ManagedFile::with_fault_hooks(home.path(), ".target", "settings", sync_hook, true)
                .unwrap();

        let error = managed
            .replace(&before, b"muxvia-desired", false)
            .unwrap_err();

        assert_eq!(error, ManagedFileError::RecoveryRequired);
        assert_eq!(fs::read(managed.path()).unwrap(), b"muxvia-desired");
        assert!(
            temporary_artifacts(&managed)
                .iter()
                .any(|path| fs::read(path).is_ok_and(|bytes| bytes == b"operator-original"))
        );
    }
}
