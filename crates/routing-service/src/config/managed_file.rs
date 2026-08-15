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
}

impl ManagedFile {
    pub(crate) fn in_configuration_home(
        user_home: &Path,
        directory_name: &str,
        file_name: &str,
    ) -> Result<Self, ManagedFileError> {
        Self::build(user_home, directory_name, file_name, None, None)
    }

    pub(crate) fn with_pre_rename_hook(
        user_home: &Path,
        directory_name: &str,
        file_name: &str,
        hook: PreRenameHook,
    ) -> Result<Self, ManagedFileError> {
        Self::build(user_home, directory_name, file_name, Some(hook), None)
    }

    pub(crate) fn with_exchange_hook(
        user_home: &Path,
        directory_name: &str,
        file_name: &str,
        hook: ExchangeHook,
    ) -> Result<Self, ManagedFileError> {
        Self::build(user_home, directory_name, file_name, None, Some(hook))
    }

    fn build(
        user_home: &Path,
        directory_name: &str,
        file_name: &str,
        pre_rename_hook: Option<PreRenameHook>,
        exchange_hook: Option<ExchangeHook>,
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
            if let Some(hook) = &self.pre_rename_hook {
                hook(&parent.join(&temporary_name)).map_err(|_| ManagedFileError::WriteFailed)?;
            }
            if !path_matches_directory(parent, directory_identity)
                || file_identity_at(&directory, target_name)? != *expected
            {
                return Err(ManagedFileError::WriteFailed);
            }
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
                    .unwrap_or(false);
                let displaced_matches = file_identity_at(&directory, temporary_name.as_str())
                    .is_ok_and(|identity| identity == *expected);
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
                        return Err(ManagedFileError::RecoveryRequired);
                    }
                    retain_temporary.set(false);
                    return Err(ManagedFileError::WriteFailed);
                }
                retain_temporary.set(false);
                if remove_file {
                    rustix::fs::unlinkat(&directory, target_name, AtFlags::empty())
                        .map_err(|_| ManagedFileError::WriteFailed)?;
                }
            } else {
                rustix::fs::renameat_with(
                    &directory,
                    temporary_name.as_str(),
                    &directory,
                    target_name,
                    RenameFlags::NOREPLACE,
                )
                .map_err(|_| ManagedFileError::WriteFailed)?;
            }
            rustix::fs::fsync(&directory).map_err(|_| ManagedFileError::WriteFailed)
        })();
        if !retain_temporary.get() {
            let _ = rustix::fs::unlinkat(&directory, temporary_name.as_str(), AtFlags::empty());
        }
        result
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
