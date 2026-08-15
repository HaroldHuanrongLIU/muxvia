use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use fs2::FileExt;
use tokio::time::MissedTickBehavior;

use crate::{
    claude::CommandClaudeProbe,
    codex::{CodexProbe, CommandCodexProbe},
    control::server::{ControlServer, ControlServerError},
    home::MuxviaHome,
    model::{ReqwestUpstream, UpstreamTransport},
    service::activate::ActivationService,
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
}

struct ServiceLock {
    _file: File,
}

impl ServiceLock {
    fn acquire(home: &MuxviaHome) -> Result<Self, ProcessError> {
        home.prepare_root()?;
        let path = home.root().join("service.lock");
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
        Ok(Self { _file: file })
    }
}

pub async fn run(options: ProcessOptions) -> Result<(), ProcessError> {
    let home = MuxviaHome::from_root(options.home)?;
    let _lock = ServiceLock::acquire(&home)?;
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
            options.codex_executable,
            upstream,
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), options.claude_executable)
        .with_configuration_home_override(std::env::var_os("CODEX_HOME").map(PathBuf::from)),
    );
    let mut control = ControlServer::bind_process(
        &home,
        Arc::clone(&store),
        options.release,
        Arc::clone(&activation),
    )
    .await?;

    if let Some(path) = options.test_shutdown_file {
        let requested = tokio::select! {
            result = control.wait_for_exit() => { result?; false },
            () = wait_for_shutdown_file(&path) => true,
        };
        if requested {
            control.request_shutdown();
        }
    } else {
        control.wait_for_exit().await?;
    }
    control.shutdown().await?;
    activation
        .shutdown_models()
        .await
        .map_err(|_| ProcessError::State)?;
    Ok(())
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
