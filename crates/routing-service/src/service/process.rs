use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use fs2::FileExt;
use tokio::time::MissedTickBehavior;

use crate::{
    claude::CommandClaudeProbe,
    codex::{CodexProbe, CommandCodexProbe},
    control::server::{ControlLifecycleOutcome, ControlServer, ControlServerError},
    home::MuxviaHome,
    model::{ReqwestUpstream, UpstreamTransport},
    service::{activate::ActivationService, handover::PreparedHandover},
    state::StateStore,
};

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("another Routing Service owns this Muxvia Home")]
    LockCollision,
    #[error("Routing Service I/O failed")]
    Io(#[from] io::Error),
    #[error("Routing Service state is unavailable")]
    State,
    #[error("Routing Service control transport failed")]
    Control(#[from] ControlServerError),
}

impl ProcessError {
    pub fn exit_code(&self) -> i32 {
        if matches!(self, Self::LockCollision) {
            73
        } else {
            1
        }
    }
}

pub struct ProcessOptions {
    pub home: PathBuf,
    pub test_shutdown_file: Option<PathBuf>,
    pub codex_executable: PathBuf,
    pub claude_executable: PathBuf,
    pub release: String,
    pub inherited_service_lock_fd: Option<i32>,
}

struct ServiceLock {
    file: File,
}

impl ServiceLock {
    fn acquire(home: &MuxviaHome, inherited_fd: Option<i32>) -> Result<Self, ProcessError> {
        let path = home.root().join("service.lock");
        if let Some(fd) = inherited_fd {
            if fd < 0 {
                return Err(ProcessError::LockCollision);
            }
            let metadata = fs::metadata(&path).map_err(|_| ProcessError::LockCollision)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `stat` points to valid writable storage and `fd` is validated by fstat.
            if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
                return Err(ProcessError::LockCollision);
            }
            // SAFETY: fstat returned success and initialized `stat`.
            let stat = unsafe { stat.assume_init() };
            if metadata.dev() != stat.st_dev as u64 || metadata.ino() != stat.st_ino {
                return Err(ProcessError::LockCollision);
            }
            // SAFETY: successful fstat proves this is an open descriptor transferred by exec.
            let file = unsafe { File::from_raw_fd(fd) };
            file.try_lock_exclusive()
                .map_err(|_| ProcessError::LockCollision)?;
            let inherited = Self { file };
            inherited.restore_close_on_exec();
            return Ok(inherited);
        }
        home.prepare_root()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        fs::set_permissions(
            home.root().join("service.lock"),
            fs::Permissions::from_mode(0o600),
        )?;
        file.try_lock_exclusive()
            .map_err(|error| match error.kind() {
                io::ErrorKind::WouldBlock => ProcessError::LockCollision,
                _ => ProcessError::Io(error),
            })?;
        Ok(Self { file })
    }

    fn prepare_for_exec(&self) -> Result<i32, ProcessError> {
        let fd = self.file.as_raw_fd();
        // SAFETY: fcntl operates on this live owned descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(ProcessError::Io(io::Error::last_os_error()));
        }
        Ok(fd)
    }

    fn restore_close_on_exec(&self) {
        let fd = self.file.as_raw_fd();
        // SAFETY: fcntl operates on this live owned descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 {
            // SAFETY: fcntl operates on this live owned descriptor.
            let _ = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }
}

pub async fn run(options: ProcessOptions) -> Result<(), ProcessError> {
    let home = MuxviaHome::from_root(options.home.clone())?;
    let lock = ServiceLock::acquire(&home, options.inherited_service_lock_fd)?;
    let store = Arc::new(
        StateStore::open(&home)
            .await
            .map_err(|_| ProcessError::State)?,
    );
    let upstream: Arc<dyn UpstreamTransport> =
        Arc::new(ReqwestUpstream::new().map_err(|_| ProcessError::State)?);
    let probe: Arc<dyn CodexProbe> = Arc::new(CommandCodexProbe);
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            probe,
            options.codex_executable.clone(),
            upstream,
        )
        .with_claude_runtime(
            Arc::new(CommandClaudeProbe),
            options.claude_executable.clone(),
        )
        .with_configuration_home_override(std::env::var_os("CODEX_HOME").map(PathBuf::from)),
    );
    loop {
        let mut control = ControlServer::bind_process(
            &home,
            Arc::clone(&store),
            options.release.clone(),
            Arc::clone(&activation),
        )
        .await?;
        let outcome = if let Some(path) = &options.test_shutdown_file {
            tokio::select! {
                result = control.wait_for_lifecycle() => result?,
                () = wait_for_shutdown_file(path) => ControlLifecycleOutcome::ExplicitShutdown,
            }
        } else {
            control.wait_for_lifecycle().await?
        };
        control.request_shutdown();
        control.shutdown().await?;
        activation
            .shutdown_models()
            .await
            .map_err(|_| ProcessError::State)?;
        match outcome {
            ControlLifecycleOutcome::Handover(prepared) => {
                let lock_fd = lock.prepare_for_exec()?;
                let _error = exec_candidate(&prepared, &options, lock_fd);
                lock.restore_close_on_exec();
            }
            ControlLifecycleOutcome::Idle | ControlLifecycleOutcome::ExplicitShutdown => {
                return Ok(());
            }
        }
    }
}

fn exec_candidate(
    prepared: &PreparedHandover,
    options: &ProcessOptions,
    inherited_lock_fd: i32,
) -> io::Error {
    let mut command = Command::new(&prepared.candidate_path);
    command
        .arg("--home")
        .arg(&options.home)
        .arg("--inherited-service-lock-fd")
        .arg(inherited_lock_fd.to_string());
    if std::env::var("MUXVIA_INTEGRATION_TEST").as_deref() == Ok("1") {
        if let Some(path) = &options.test_shutdown_file {
            command.arg("--test-shutdown-file").arg(path);
        }
        command
            .arg("--test-codex-executable")
            .arg(&options.codex_executable)
            .arg("--test-claude-executable")
            .arg(&options.claude_executable);
    }
    command.exec()
}

async fn wait_for_shutdown_file(path: &Path) {
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
            return;
        }
    }
}
