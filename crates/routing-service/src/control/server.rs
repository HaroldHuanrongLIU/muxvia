use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde_json::Value;
use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{broadcast, oneshot},
    task::{JoinHandle, JoinSet},
};

use crate::{
    codex::CodexConfigCodec,
    control::{
        framing::{read_frame, write_frame},
        protocol::{
            ClientFrame, ControlOperation, ControlProblem, ControlResult, FrameLimit, RpcVersion,
            ServerFrame, TargetView,
        },
    },
    home::MuxviaHome,
    state::StateStore,
};

#[derive(Debug, Error)]
pub enum ControlServerError {
    #[error("control socket path collides with a non-socket entry")]
    UnsafeSocketCollision,
    #[error("control server I/O failed")]
    Io(#[from] io::Error),
    #[error("state store unavailable")]
    State,
    #[error("control server task failed")]
    Task,
}

pub struct ControlServer;

pub struct ControlServerHandle {
    socket_path: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl ControlServer {
    pub async fn bind(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
    ) -> Result<ControlServerHandle, ControlServerError> {
        let codec = CodexConfigCodec::for_user_home(home.user_home())
            .map_err(|_| ControlServerError::State)?;
        if codec.reconcile_pending(&store).await.is_err()
            && store
                .target_view()
                .await
                .map_err(|_| ControlServerError::State)?
                .recovery
                .state
                != "recovery-required"
        {
            return Err(ControlServerError::State);
        }

        let run_dir = home.root().join("run");
        fs::create_dir_all(&run_dir)?;
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))?;

        let socket_path = run_dir.join("control.sock");
        remove_stale_socket(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

        let release = release.into();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let task_path = socket_path.clone();
        let task = tokio::spawn(async move {
            let mut sessions = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    _ = sessions.join_next(), if !sessions.is_empty() => {}
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let store = Arc::clone(&store);
                        let release = release.clone();
                        sessions.spawn(async move {
                            serve_authorized(stream, store, release).await;
                        });
                    }
                }
            }
            sessions.abort_all();
            while sessions.join_next().await.is_some() {}
            remove_socket_if_present(&task_path);
        });

        Ok(ControlServerHandle {
            socket_path,
            shutdown: Some(shutdown_tx),
            task: Some(task),
        })
    }
}

impl ControlServerHandle {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn shutdown(mut self) -> Result<(), ControlServerError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.map_err(|_| ControlServerError::Task)?;
        }
        Ok(())
    }
}

impl Drop for ControlServerHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub fn peer_uid_matches(peer_uid: u32, effective_uid: u32) -> bool {
    peer_uid == effective_uid
}

fn remove_stale_socket(path: &Path) -> Result<(), ControlServerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => return Err(ControlServerError::UnsafeSocketCollision),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn remove_socket_if_present(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        let _ = fs::remove_file(path);
    }
}

async fn serve_authorized(mut stream: UnixStream, store: Arc<StateStore>, release: String) {
    let authorized = stream.peer_cred().is_ok_and(|credentials| {
        // SAFETY: geteuid has no preconditions and only reads the process credential.
        peer_uid_matches(credentials.uid(), unsafe { libc::geteuid() })
    });
    if !authorized {
        let _ = write_problem(
            &mut stream,
            None,
            "unauthorized-peer",
            "Unauthorized peer",
            None,
        )
        .await;
        return;
    }

    serve_session(&mut stream, store, release).await;
}

async fn serve_session(stream: &mut UnixStream, store: Arc<StateStore>, release: String) {
    let Ok(first) = read_frame(stream).await else {
        return;
    };
    if first.get("type").and_then(Value::as_str) != Some("hello") {
        let request_id = request_id(&first);
        let _ = write_problem(
            stream,
            request_id,
            "handshake-required",
            "Hello must be the first frame",
            None,
        )
        .await;
        return;
    }
    if !compatible_hello(&first) {
        let _ = write_problem(
            stream,
            None,
            "protocol-mismatch",
            "Unsupported control protocol version",
            None,
        )
        .await;
        return;
    }
    if serde_json::from_value::<ClientFrame>(first).is_err() {
        let _ = write_problem(
            stream,
            None,
            "invalid-request",
            "Malformed hello frame",
            None,
        )
        .await;
        return;
    }

    let Ok(initial_view) = store.target_view().await else {
        let _ = write_problem(
            stream,
            None,
            "state-store-error",
            "State store unavailable",
            None,
        )
        .await;
        return;
    };
    let ack = ServerFrame::HelloAck {
        rpc: RpcVersion::V1_0,
        release,
        service_epoch: initial_view.service.epoch,
        frame_limit: FrameLimit::V1,
    };
    if write_frame(stream, &ack).await.is_err() {
        return;
    }

    let mut subscribed = false;
    let mut update_rx = store.subscribe_target_views();
    loop {
        tokio::select! {
            incoming = read_frame(stream) => {
                let Ok(raw) = incoming else { return };
                if raw.get("type").and_then(Value::as_str) == Some("hello") {
                    if write_problem(stream, None, "unexpected-hello", "Hello was already negotiated", None).await.is_err() {
                        return;
                    }
                    continue;
                }
                let request_id = request_id(&raw);
                let parsed = serde_json::from_value::<ClientFrame>(raw.clone());
                let Ok(ClientFrame::Request { request_id, operation }) = parsed else {
                    let code = if raw
                        .get("operation")
                        .and_then(|operation| operation.get("kind"))
                        .and_then(Value::as_str)
                        .is_some()
                    {
                        "unsupported-operation"
                    } else {
                        "invalid-request"
                    };
                    if write_problem(stream, request_id, code, "Unsupported or malformed request", None).await.is_err() {
                        return;
                    }
                    continue;
                };

                match operation {
                    ControlOperation::OpenTarget { .. } => {
                        let Ok(view) = store.target_view().await else {
                            if write_problem(stream, Some(request_id), "state-store-error", "State store unavailable", None).await.is_err() {
                                return;
                            }
                            continue;
                        };
                        subscribed = true;
                        let response = ServerFrame::Response {
                            request_id,
                            result: ControlResult::TargetView { view },
                        };
                        if write_frame(stream, &response).await.is_err() { return; }
                    }
                    ControlOperation::Act { action_id, expected_revision, action, .. } => {
                        match store.apply_save_provider_action(action_id, expected_revision, action).await {
                            Ok(outcome) => {
                                let view = outcome.view.clone();
                                let response = ServerFrame::Response {
                                    request_id,
                                    result: ControlResult::ActionOutcome { outcome },
                                };
                                if write_frame(stream, &response).await.is_err() { return; }
                                store.publish_target_view(view);
                            }
                            Err(failure) => {
                                if write_problem(
                                    stream,
                                    Some(request_id),
                                    &failure.problem.code,
                                    &failure.problem.message,
                                    Some(failure.authoritative_view),
                                ).await.is_err() { return; }
                            }
                        }
                    }
                }
            }
            update = update_rx.recv(), if subscribed => {
                match update {
                    Ok(view) => {
                        if write_frame(stream, &ServerFrame::TargetView { view }).await.is_err() { return; }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Ok(view) = store.target_view().await else { continue };
                        if write_frame(stream, &ServerFrame::TargetView { view }).await.is_err() { return; }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

fn compatible_hello(value: &Value) -> bool {
    value
        .get("rpc")
        .and_then(|rpc| rpc.get("major"))
        .and_then(Value::as_u64)
        == Some(1)
        && value
            .get("rpc")
            .and_then(|rpc| rpc.get("minor"))
            .and_then(Value::as_u64)
            == Some(0)
        && value.get("release").and_then(Value::as_str).is_some()
}

fn request_id(value: &Value) -> Option<String> {
    value
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

async fn write_problem(
    stream: &mut UnixStream,
    request_id: Option<String>,
    code: &str,
    message: &str,
    authoritative_view: Option<TargetView>,
) -> Result<(), crate::control::framing::FrameError> {
    write_frame(
        stream,
        &ServerFrame::Error {
            request_id,
            problem: ControlProblem {
                code: code.to_owned(),
                message: message.to_owned(),
            },
            authoritative_view,
        },
    )
    .await
}
