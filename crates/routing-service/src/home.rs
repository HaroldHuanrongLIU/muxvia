use std::{
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

#[derive(Clone)]
pub struct MuxviaHome {
    user_home: PathBuf,
    root: PathBuf,
    state: PathBuf,
    database: PathBuf,
}

impl MuxviaHome {
    pub fn from_user_home(user_home: &Path) -> Self {
        let root = user_home.join(".muxvia");
        let state = root.join("state");
        let database = state.join("muxvia.db");
        Self {
            user_home: user_home.to_path_buf(),
            root,
            state,
            database,
        }
    }

    pub fn from_root(root: PathBuf) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Muxvia Home must be absolute",
            ));
        }
        let user_home = root.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Muxvia Home has no parent")
        })?;
        let state = root.join("state");
        let database = state.join("muxvia.db");
        Ok(Self {
            user_home: user_home.to_owned(),
            root,
            state,
            database,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn user_home(&self) -> &Path {
        &self.user_home
    }

    pub fn state_dir(&self) -> &Path {
        &self.state
    }

    pub fn database_path(&self) -> &Path {
        &self.database
    }

    pub fn subscription_accounts_path(&self) -> PathBuf {
        self.state.join("subscription-accounts.json")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    pub(crate) fn prepare_backups_dir(&self) -> io::Result<PathBuf> {
        self.prepare_root()?;
        let backups = self.backups_dir();
        match fs::symlink_metadata(&backups) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Muxvia backups path must be a directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&backups)?;
            }
            Err(error) => return Err(error),
        }
        #[cfg(unix)]
        fs::set_permissions(&backups, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
        Ok(backups)
    }

    pub(crate) fn prepare_database(&self) -> io::Result<()> {
        create_private_dir(&self.root)?;
        create_private_dir(&self.state)?;

        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        options.mode(PRIVATE_FILE_MODE);
        options.open(&self.database)?;
        set_private_file_permissions(&self.database)
    }

    pub(crate) fn prepare_root(&self) -> io::Result<()> {
        create_private_dir(&self.root)
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    Ok(())
}
