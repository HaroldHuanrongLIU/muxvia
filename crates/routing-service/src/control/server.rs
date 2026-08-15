use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::Value;
use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{broadcast, mpsc, oneshot, watch},
    task::{Id, JoinHandle, JoinSet},
};

use crate::{
    claude::ClaudeConfigCodec,
    codex::{CodexConfigCodec, CommandCodexProbe},
    control::{
        framing::{FrameError, read_frame, write_frame},
        protocol::{
            ClientFrame, ControlOperation, ControlProblem, ControlResult, DiscoverySource,
            FrameLimit, RpcVersion, ServerFrame, Target, TargetView,
        },
    },
    domain::provider::has_valid_provider_authentication,
    home::MuxviaHome,
    model::ReqwestUpstream,
    service::{activate::ActivationService, provider_inspector::ProviderInspector},
    state::{ManagedWriteStatus, StateStore},
};

const RESPONSE_QUEUE_CAPACITY: usize = 32;
const MAX_IN_FLIGHT_INSPECTIONS_PER_SESSION: usize = 4;

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
    completed: watch::Receiver<bool>,
    lifecycle: Arc<ServerLifecycle>,
}

#[derive(Default)]
struct ServerLifecycle {
    accepted: AtomicBool,
    active_sessions: AtomicUsize,
    pending_actions: AtomicUsize,
    pending_inspections: AtomicUsize,
}

struct SessionGuard(Arc<ServerLifecycle>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.active_sessions.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ActionGuard(Arc<ServerLifecycle>);

impl Drop for ActionGuard {
    fn drop(&mut self) {
        self.0.pending_actions.fetch_sub(1, Ordering::AcqRel);
    }
}

struct InspectionGuard(Arc<ServerLifecycle>);

impl Drop for InspectionGuard {
    fn drop(&mut self) {
        self.0.pending_inspections.fetch_sub(1, Ordering::AcqRel);
    }
}

struct InspectionRequest {
    task_id: Id,
    cancel: Option<oneshot::Sender<()>>,
}

struct InspectionCompletion {
    request_id: String,
    disposition: InspectionDisposition,
}

enum InspectionDisposition {
    Written,
    Cancelled,
    CloseSession,
}

struct QueuedResponse {
    frame: ServerFrame,
    written: Option<oneshot::Sender<()>>,
}

impl ControlServer {
    pub async fn bind(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
    ) -> Result<ControlServerHandle, ControlServerError> {
        let upstream = ReqwestUpstream::new().map_err(|_| ControlServerError::State)?;
        let executable = find_codex_executable();
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(CommandCodexProbe),
                executable,
                Arc::new(upstream),
            )
            .with_configuration_home_override(std::env::var_os("CODEX_HOME").map(PathBuf::from)),
        );
        Self::bind_configured(home, store, release, activation, false).await
    }

    pub async fn bind_with_activation(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
        activation: Arc<ActivationService>,
    ) -> Result<ControlServerHandle, ControlServerError> {
        Self::bind_configured(home, store, release, activation, false).await
    }

    pub async fn bind_process(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
        activation: Arc<ActivationService>,
    ) -> Result<ControlServerHandle, ControlServerError> {
        Self::bind_configured(home, store, release, activation, true).await
    }

    async fn bind_configured(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
        activation: Arc<ActivationService>,
        exit_when_idle: bool,
    ) -> Result<ControlServerHandle, ControlServerError> {
        let inspector = Arc::new(
            ProviderInspector::new(Arc::clone(&store)).map_err(|_| ControlServerError::State)?,
        );
        for target in [Target::Codex, Target::Claude] {
            let reconciled = match target {
                Target::Codex => match CodexConfigCodec::for_user_home(home.user_home()) {
                    Ok(codec) => Some(codec.reconcile_pending(&store).await.is_ok()),
                    Err(_) => None,
                },
                Target::Claude => match ClaudeConfigCodec::for_user_home(home.user_home()) {
                    Ok(codec) => Some(codec.reconcile_pending(&store).await.is_ok()),
                    Err(_) => None,
                },
            };
            let reconciled = match reconciled {
                Some(reconciled) => reconciled,
                None => {
                    store
                        .record_startup_problem_for(
                            target,
                            "model-route-unavailable",
                            "The committed model route could not be resumed",
                        )
                        .await
                        .map_err(|_| ControlServerError::State)?;
                    continue;
                }
            };
            match store
                .managed_write_status_for(target)
                .await
                .map_err(|_| ControlServerError::State)?
            {
                ManagedWriteStatus::Allowed if reconciled => {
                    store
                        .clear_startup_problems_for(target)
                        .await
                        .map_err(|_| ControlServerError::State)?;
                }
                ManagedWriteStatus::RecoveryRequired | ManagedWriteStatus::ConfigurationDrift => {}
                ManagedWriteStatus::Allowed => {
                    store
                        .record_startup_problem_for(
                            target,
                            "startup-reconciliation-failed",
                            "Managed configuration recovery could not be reconciled",
                        )
                        .await
                        .map_err(|_| ControlServerError::State)?;
                }
            }
        }
        activation
            .bootstrap_committed_takeovers()
            .await
            .map_err(|_| ControlServerError::State)?;

        let run_dir = home.root().join("run");
        fs::create_dir_all(&run_dir)?;
        fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))?;

        let socket_path = run_dir.join("control.sock");
        remove_stale_socket(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

        let release = release.into();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let (session_shutdown_tx, session_shutdown_rx) = watch::channel(false);
        let (completed_tx, completed) = watch::channel(false);
        let lifecycle = Arc::new(ServerLifecycle::default());
        let handle_lifecycle = Arc::clone(&lifecycle);
        let task_path = socket_path.clone();
        let task = tokio::spawn(async move {
            let mut sessions = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        let _ = session_shutdown_tx.send(true);
                        break;
                    }
                    _ = sessions.join_next(), if !sessions.is_empty() => {
                        if exit_when_idle && should_exit_idle(&store, &lifecycle).await {
                            let _ = session_shutdown_tx.send(true);
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        lifecycle.accepted.store(true, Ordering::Release);
                        lifecycle.active_sessions.fetch_add(1, Ordering::AcqRel);
                        let store = Arc::clone(&store);
                        let activation = Arc::clone(&activation);
                        let inspector = Arc::clone(&inspector);
                        let release = release.clone();
                        let lifecycle = Arc::clone(&lifecycle);
                        let session_shutdown = session_shutdown_rx.clone();
                        sessions.spawn(async move {
                            let _guard = SessionGuard(Arc::clone(&lifecycle));
                            serve_authorized(
                                stream,
                                store,
                                activation,
                                inspector,
                                release,
                                lifecycle,
                                session_shutdown,
                            ).await;
                        });
                    }
                }
            }
            drop(listener);
            while sessions.join_next().await.is_some() {}
            remove_socket_if_present(&task_path);
            let _ = completed_tx.send(true);
        });

        Ok(ControlServerHandle {
            socket_path,
            shutdown: Some(shutdown_tx),
            task: Some(task),
            completed,
            lifecycle: handle_lifecycle,
        })
    }
}

impl ControlServerHandle {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[doc(hidden)]
    pub fn tracked_inspections(&self) -> usize {
        self.lifecycle.pending_inspections.load(Ordering::Acquire)
    }

    pub fn request_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    pub async fn wait_for_exit(&mut self) -> Result<(), ControlServerError> {
        while !*self.completed.borrow() {
            self.completed
                .changed()
                .await
                .map_err(|_| ControlServerError::Task)?;
        }
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<(), ControlServerError> {
        self.request_shutdown();
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

async fn serve_authorized(
    mut stream: UnixStream,
    store: Arc<StateStore>,
    activation: Arc<ActivationService>,
    inspector: Arc<ProviderInspector>,
    release: String,
    lifecycle: Arc<ServerLifecycle>,
    shutdown: watch::Receiver<bool>,
) {
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

    serve_session(
        stream, store, activation, inspector, release, lifecycle, shutdown,
    )
    .await;
}

async fn serve_session(
    mut stream: UnixStream,
    store: Arc<StateStore>,
    activation: Arc<ActivationService>,
    inspector: Arc<ProviderInspector>,
    release: String,
    lifecycle: Arc<ServerLifecycle>,
    mut shutdown: watch::Receiver<bool>,
) {
    let first = match read_frame(&mut stream).await {
        Ok(first) => first,
        Err(FrameError::EndOfStream | FrameError::Io) => return,
        Err(_) => {
            let _ = write_frame_invalid(&mut stream).await;
            return;
        }
    };
    if first.get("type").and_then(Value::as_str) != Some("hello") {
        let request_id = request_id(&first);
        let _ = write_problem(
            &mut stream,
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
            &mut stream,
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
            &mut stream,
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
            &mut stream,
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
    if write_frame(&mut stream, &ack).await.is_err() {
        return;
    }

    let (mut reader, writer) = stream.into_split();
    let (responses, response_rx) = mpsc::channel(RESPONSE_QUEUE_CAPACITY);
    let mut writer_task = tokio::spawn(write_responses(writer, response_rx));
    let mut opened_target = None;
    let mut opened_claude_context = None;
    let mut update_rx = store.subscribe_target_views();
    let mut inspections = JoinSet::<InspectionCompletion>::new();
    let mut inspection_requests = std::collections::HashMap::<String, InspectionRequest>::new();
    'session: loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break 'session; }
            }
            completed = inspections.join_next_with_id(), if !inspections.is_empty() => {
                match completed {
                    Some(Ok((_, completion))) => {
                        inspection_requests.remove(&completion.request_id);
                        if matches!(completion.disposition, InspectionDisposition::CloseSession) {
                            break 'session;
                        }
                    }
                    Some(Err(failure)) => {
                        let task_id = failure.id();
                        inspection_requests.retain(|_, request| request.task_id != task_id);
                    }
                    None => {}
                }
            }
            incoming = read_frame(&mut reader) => {
                let raw = match incoming {
                    Ok(raw) => raw,
                    Err(FrameError::EndOfStream | FrameError::Io) => break 'session,
                    Err(_) => {
                        let _ = enqueue_response(&responses, problem_frame(None, "frame-invalid", "Control frame is invalid", None));
                        break 'session;
                    }
                };
                if raw.get("type").and_then(Value::as_str) == Some("hello") {
                    if !enqueue_response(&responses, problem_frame(None, "unexpected-hello", "Hello was already negotiated", None)) {
                        break 'session;
                    }
                    continue;
                }
                let request_id = request_id(&raw);
                let parsed = serde_json::from_value::<ClientFrame>(raw.clone());
                let (request_id, operation) = match parsed {
                    Ok(ClientFrame::Cancel { request_id }) => {
                        if let Some(request) = inspection_requests.get_mut(&request_id)
                            && let Some(cancel) = request.cancel.take()
                        {
                            let _ = cancel.send(());
                        }
                        continue;
                    }
                    Ok(ClientFrame::Request { request_id, operation }) => (request_id, operation),
                    _ => {
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
                        if !enqueue_response(&responses, problem_frame(request_id, code, "Unsupported or malformed request", None)) {
                            break 'session;
                        }
                        continue;
                    }
                };

                let target = operation_target(&operation);
                if opened_target.is_none() && !matches!(operation, ControlOperation::OpenTarget { .. }) {
                    if !enqueue_response(&responses, problem_frame(
                        Some(request_id),
                        "target-not-open",
                        "Open a Target before issuing operations",
                        None,
                    )) {
                        break 'session;
                    }
                    continue;
                }
                if let Some(opened_target) = opened_target
                    && opened_target != target
                {
                    if !enqueue_response(&responses, problem_frame(
                        Some(request_id),
                        "target-session-target-mismatch",
                        "Operation does not match the opened Target",
                        None,
                    )) {
                        break 'session;
                    }
                    continue;
                }

                match operation {
                    ControlOperation::OpenTarget { target, claude_context } => {
                        let Ok(view) = store.target_view_for(target).await else {
                            if !enqueue_response(&responses, problem_frame(Some(request_id), "state-store-error", "State store unavailable", None)) {
                                break 'session;
                            }
                            continue;
                        };
                        opened_target = Some(target);
                        opened_claude_context = if target == Target::Claude {
                            claude_context.or(opened_claude_context)
                        } else {
                            None
                        };
                        let response = ServerFrame::Response {
                            request_id,
                            result: ControlResult::TargetView { view },
                        };
                        if !enqueue_response(&responses, response) { break 'session; }
                    }
                    ControlOperation::Act { target, action_id, expected_revision, action } => {
                        lifecycle.pending_actions.fetch_add(1, Ordering::AcqRel);
                        let _action = ActionGuard(Arc::clone(&lifecycle));
                        match activation
                            .apply_raw_for_with_context(
                                target,
                                action_id,
                                expected_revision,
                                action,
                                opened_claude_context.as_ref(),
                            )
                            .await
                        {
                            Ok(outcome) => {
                                let response = ServerFrame::Response {
                                    request_id,
                                    result: ControlResult::ActionOutcome { outcome },
                                };
                                if !enqueue_response(&responses, response) { break 'session; }
                            }
                            Err(failure) => {
                                if !enqueue_response(&responses, ServerFrame::Error {
                                    request_id: Some(request_id),
                                    problem: failure.problem,
                                    authoritative_view: Some(failure.authoritative_view),
                                }) { break 'session; }
                            }
                        }
                    }
                    operation @ (ControlOperation::DiscoverModels { .. }
                    | ControlOperation::CheckReachability { .. }) => {
                        if let ControlOperation::DiscoverModels {
                            source: DiscoverySource::Draft { authentication, .. },
                            ..
                        } = &operation
                            && !has_valid_provider_authentication(target, *authentication)
                        {
                            if !enqueue_response(&responses, problem_frame(
                                Some(request_id),
                                "invalid-provider-authentication",
                                "Draft authentication does not match Target",
                                None,
                            )) {
                                break 'session;
                            }
                            continue;
                        }
                        if inspection_requests.contains_key(&request_id) {
                            let _ = enqueue_response(&responses, problem_frame(
                                Some(request_id),
                                "request-in-progress",
                                "Request identifier is already in progress",
                                None,
                            ));
                            break 'session;
                        }
                        if inspections.len() >= MAX_IN_FLIGHT_INSPECTIONS_PER_SESSION {
                            if !enqueue_response(&responses, problem_frame(
                                Some(request_id),
                                "inspection-limit-reached",
                                "Too many inspections are already in progress",
                                None,
                            )) {
                                break 'session;
                            }
                            continue;
                        }
                        lifecycle.pending_inspections.fetch_add(1, Ordering::AcqRel);
                        let guard = InspectionGuard(Arc::clone(&lifecycle));
                        let inspector = Arc::clone(&inspector);
                        let responses = responses.clone();
                        let task_request_id = request_id.clone();
                        let (cancel, cancelled) = oneshot::channel();
                        let abort = inspections.spawn(inspect_and_queue(
                            task_request_id,
                            target,
                            operation,
                            inspector,
                            responses,
                            cancelled,
                            guard,
                        ));
                        inspection_requests.insert(request_id, InspectionRequest {
                            task_id: abort.id(),
                            cancel: Some(cancel),
                        });
                    }
                }
            }
            update = update_rx.recv(), if opened_target.is_some() => {
                match update {
                    Ok(view) => {
                        if Some(view.target) != opened_target { continue; }
                        if !enqueue_response(&responses, ServerFrame::TargetView { view }) { break 'session; }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(target) = opened_target else { continue; };
                        let Ok(view) = store.target_view_for(target).await else { continue };
                        if !enqueue_response(&responses, ServerFrame::TargetView { view }) { break 'session; }
                    }
                    Err(broadcast::error::RecvError::Closed) => break 'session,
                }
            }
        }
    }

    inspections.abort_all();
    inspection_requests.clear();
    while inspections.join_next().await.is_some() {}
    drop(responses);
    if tokio::time::timeout(Duration::from_millis(250), &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
        let _ = writer_task.await;
    }
}

fn operation_target(operation: &ControlOperation) -> Target {
    match operation {
        ControlOperation::OpenTarget { target, .. }
        | ControlOperation::Act { target, .. }
        | ControlOperation::DiscoverModels { target, .. }
        | ControlOperation::CheckReachability { target, .. } => *target,
    }
}

async fn inspect_and_queue(
    request_id: String,
    target: Target,
    operation: ControlOperation,
    inspector: Arc<ProviderInspector>,
    responses: mpsc::Sender<QueuedResponse>,
    cancelled: oneshot::Receiver<()>,
    guard: InspectionGuard,
) -> InspectionCompletion {
    let _guard = guard;
    let inspection = async {
        match operation {
            ControlOperation::DiscoverModels { source, .. } => ControlResult::ModelDiscovery {
                result: inspector.discover_models_for(target, source).await,
            },
            ControlOperation::CheckReachability {
                provider_id,
                provider_revision,
                ..
            } => ControlResult::Reachability {
                result: inspector
                    .check_reachability_for(target, provider_id, provider_revision)
                    .await,
            },
            _ => unreachable!(),
        }
    };
    let result = tokio::select! {
        biased;
        _ = cancelled => {
            return InspectionCompletion {
                request_id,
                disposition: InspectionDisposition::Cancelled,
            };
        }
        result = inspection => result,
    };

    let (written, write_acknowledged) = oneshot::channel();
    if responses
        .try_send(QueuedResponse {
            frame: ServerFrame::Response {
                request_id: request_id.clone(),
                result,
            },
            written: Some(written),
        })
        .is_err()
    {
        return InspectionCompletion {
            request_id,
            disposition: InspectionDisposition::CloseSession,
        };
    }
    let disposition = if write_acknowledged.await.is_ok() {
        InspectionDisposition::Written
    } else {
        InspectionDisposition::CloseSession
    };
    InspectionCompletion {
        request_id,
        disposition,
    }
}

fn enqueue_response(responses: &mpsc::Sender<QueuedResponse>, frame: ServerFrame) -> bool {
    responses
        .try_send(QueuedResponse {
            frame,
            written: None,
        })
        .is_ok()
}

async fn write_responses(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut responses: mpsc::Receiver<QueuedResponse>,
) {
    while let Some(response) = responses.recv().await {
        if write_frame(&mut writer, &response.frame).await.is_err() {
            return;
        }
        if let Some(written) = response.written {
            let _ = written.send(());
        }
    }
}

fn problem_frame(
    request_id: Option<String>,
    code: &str,
    message: &str,
    authoritative_view: Option<TargetView>,
) -> ServerFrame {
    ServerFrame::Error {
        request_id,
        problem: ControlProblem {
            code: code.to_owned(),
            message: message.to_owned(),
            source: None,
            selector: None,
        },
        authoritative_view,
    }
}

async fn should_exit_idle(store: &StateStore, lifecycle: &ServerLifecycle) -> bool {
    lifecycle.accepted.load(Ordering::Acquire)
        && lifecycle.active_sessions.load(Ordering::Acquire) == 0
        && lifecycle.pending_actions.load(Ordering::Acquire) == 0
        && matches!(store.service_lifecycle_required().await, Ok(false))
}

fn find_codex_executable() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("codex"))
                .find(|candidate| candidate.is_file())
        })
        .and_then(|path| fs::canonicalize(path).ok())
        .unwrap_or_else(|| PathBuf::from("/usr/bin/codex"))
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
                source: None,
                selector: None,
            },
            authoritative_view,
        },
    )
    .await
}

async fn write_frame_invalid(
    stream: &mut UnixStream,
) -> Result<(), crate::control::framing::FrameError> {
    write_problem(
        stream,
        None,
        "frame-invalid",
        "Control frame is invalid",
        None,
    )
    .await
}
