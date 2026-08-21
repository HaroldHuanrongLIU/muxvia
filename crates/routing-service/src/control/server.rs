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
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    claude::ClaudeConfigCodec,
    codex::{CodexConfigCodec, CommandCodexProbe},
    control::{
        framing::{FrameError, read_frame, write_frame},
        protocol::{
            ActionStatus, ClaudePreflightContext, ClientFrame, CompatibilityProbeResult,
            ControlOperation, ControlProblem, ControlResult, DiscoverySource, FrameLimit,
            HandoverPreparedResult, ReconciliationStrategy, RpcVersion, ServerFrame, Target,
            TargetAction, TargetView, UniversalProviderAction, UniversalProviderCatalogView,
        },
    },
    domain::provider::has_valid_provider_authentication,
    home::MuxviaHome,
    model::ReqwestUpstream,
    native_usage::{NativeUsageError, NativeUsageService},
    service::{
        activate::ActivationService,
        handover::{PreparedHandover, probe_candidate},
        provider_inspector::ProviderInspector,
        provider_synchronization::ProviderSynchronizationService,
        reconcile::ReconciliationService,
        reconciliation_adapter::ReconciliationContext,
        route_plan::RoutePlanCoordinator,
    },
    state::{ManagedWriteStatus, StateStore},
    subscription::{
        DeviceAuthorizationManager, ReqwestDeviceAuthorizationAuthority,
        SubscriptionAccountCoordinator, SubscriptionAccountStore,
        SubscriptionAuthorizationCancellation, SubscriptionAuthorizationCommit,
    },
};

const RESPONSE_QUEUE_CAPACITY: usize = 32;
const MAX_IN_FLIGHT_INSPECTIONS_PER_SESSION: usize = 4;
const FRAME_WRITE_TIMEOUT: Duration = Duration::from_millis(250);

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
    reconciliation: Arc<ReconciliationService>,
    handover: Option<mpsc::Receiver<PreparedHandover>>,
}

pub(crate) enum ControlLifecycleOutcome {
    Idle,
    ExplicitShutdown,
    Handover(PreparedHandover),
}

#[derive(Default)]
struct ServerLifecycle {
    accepted: AtomicBool,
    active_sessions: AtomicUsize,
    active_writers: AtomicUsize,
    pending_actions: AtomicUsize,
    pending_inspections: AtomicUsize,
}

struct SessionGuard(Arc<ServerLifecycle>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.active_sessions.fetch_sub(1, Ordering::AcqRel);
    }
}

struct WriterGuard(Option<Arc<ServerLifecycle>>);

impl WriterGuard {
    fn new(lifecycle: Arc<ServerLifecycle>) -> Self {
        lifecycle.active_writers.fetch_add(1, Ordering::AcqRel);
        Self(Some(lifecycle))
    }

    fn closed(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if let Some(lifecycle) = self.0.take() {
            lifecycle.active_writers.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.release();
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
    cancellation: InspectionCancellation,
}

#[derive(Clone)]
enum InspectionCancellation {
    Standard(CancellationToken),
    SubscriptionAuthorization(SubscriptionAuthorizationCancellation),
}

impl InspectionCancellation {
    async fn cancel(&self) {
        match self {
            Self::Standard(cancellation) => cancellation.cancel(),
            Self::SubscriptionAuthorization(cancellation) => cancellation.cancel().await,
        }
    }
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

#[derive(Clone)]
struct SessionServices {
    store: Arc<StateStore>,
    activation: Arc<ActivationService>,
    inspector: Arc<ProviderInspector>,
    reconciliation: Arc<ReconciliationService>,
    route_plans: Arc<RoutePlanCoordinator>,
    provider_synchronization: Arc<ProviderSynchronizationService>,
    device_authorization: Arc<DeviceAuthorizationManager>,
    subscription_account_coordinator: Arc<SubscriptionAccountCoordinator>,
    native_usage: Arc<NativeUsageService>,
    handover: Option<mpsc::Sender<PreparedHandover>>,
}

struct InspectionOperation {
    target: Target,
    operation: ControlOperation,
    opened_claude_context: Option<crate::control::protocol::ClaudePreflightContext>,
}

#[derive(Clone)]
struct InspectionServices {
    inspector: Arc<ProviderInspector>,
    reconciliation: Arc<ReconciliationService>,
    store: Arc<StateStore>,
    native_usage: Arc<NativeUsageService>,
}

#[derive(Clone, Copy)]
struct ServerRuntime {
    exit_when_idle: bool,
    native_usage_scan_interval: Duration,
}

impl ServerRuntime {
    fn session() -> Self {
        Self {
            exit_when_idle: false,
            native_usage_scan_interval: Duration::from_secs(60),
        }
    }

    fn process(native_usage_scan_interval: Duration) -> Self {
        Self {
            exit_when_idle: true,
            native_usage_scan_interval,
        }
    }
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
        Self::bind_configured(
            home,
            store,
            release,
            activation,
            None,
            None,
            ServerRuntime::session(),
        )
        .await
    }

    pub async fn bind_with_activation(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
        activation: Arc<ActivationService>,
    ) -> Result<ControlServerHandle, ControlServerError> {
        Self::bind_configured(
            home,
            store,
            release,
            activation,
            None,
            None,
            ServerRuntime::session(),
        )
        .await
    }

    #[doc(hidden)]
    pub async fn bind_with_activation_and_device_authority_origin(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
        activation: Arc<ActivationService>,
        authority_origin: &str,
    ) -> Result<ControlServerHandle, ControlServerError> {
        let authority = Arc::new(
            ReqwestDeviceAuthorizationAuthority::for_origin(authority_origin)
                .map_err(|_| ControlServerError::State)?,
        );
        Self::bind_configured(
            home,
            store,
            release,
            activation,
            Some(authority),
            None,
            ServerRuntime::session(),
        )
        .await
    }

    pub async fn bind_process(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
        activation: Arc<ActivationService>,
    ) -> Result<ControlServerHandle, ControlServerError> {
        Self::bind_configured(
            home,
            store,
            release,
            activation,
            None,
            None,
            ServerRuntime::process(Duration::from_secs(60)),
        )
        .await
    }

    #[doc(hidden)]
    pub async fn bind_process_with_native_usage_scan_interval(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
        activation: Arc<ActivationService>,
        scan_interval: Duration,
    ) -> Result<ControlServerHandle, ControlServerError> {
        Self::bind_configured(
            home,
            store,
            release,
            activation,
            None,
            None,
            ServerRuntime::process(scan_interval),
        )
        .await
    }

    #[doc(hidden)]
    pub async fn bind_process_with_device_authority_origin(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
        activation: Arc<ActivationService>,
        authority_origin: &str,
        refresh_account_id: Option<String>,
    ) -> Result<ControlServerHandle, ControlServerError> {
        let authority = Arc::new(
            ReqwestDeviceAuthorizationAuthority::for_origin(authority_origin)
                .map_err(|_| ControlServerError::State)?,
        );
        Self::bind_configured(
            home,
            store,
            release,
            activation,
            Some(authority),
            refresh_account_id,
            ServerRuntime::process(Duration::from_secs(60)),
        )
        .await
    }

    async fn bind_configured(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        release: impl Into<String>,
        activation: Arc<ActivationService>,
        device_authority: Option<
            Arc<dyn crate::subscription::device_authorization::DeviceAuthorizationAuthority>,
        >,
        startup_refresh_account_id: Option<String>,
        runtime: ServerRuntime,
    ) -> Result<ControlServerHandle, ControlServerError> {
        let reconciliation_runtime = activation.reconciliation_runtime();
        if reconciliation_runtime.home.root() != home.root() {
            return Err(ControlServerError::State);
        }
        let inspector = Arc::new(
            ProviderInspector::new(Arc::clone(&store)).map_err(|_| ControlServerError::State)?,
        );
        let provider_synchronization = Arc::new(ProviderSynchronizationService::from_runtime(
            Arc::clone(&store),
            reconciliation_runtime.clone(),
        ));
        let reconciliation = Arc::new(ReconciliationService::from_runtime(
            Arc::clone(&store),
            reconciliation_runtime,
        ));
        let route_plans = Arc::new(RoutePlanCoordinator::new(
            Arc::clone(&store),
            Arc::clone(&reconciliation),
        ));
        let subscription_accounts =
            Arc::new(SubscriptionAccountStore::open(home).map_err(|_| ControlServerError::State)?);
        let device_authority = match device_authority {
            Some(authority) => authority,
            None => Arc::new(
                ReqwestDeviceAuthorizationAuthority::new()
                    .map_err(|_| ControlServerError::State)?,
            ),
        };
        let subscription_account_coordinator = Arc::new(SubscriptionAccountCoordinator::new(
            Arc::clone(&store),
            Arc::clone(&subscription_accounts),
        ));
        let device_authorization = Arc::new(DeviceAuthorizationManager::new(
            Arc::clone(&subscription_accounts),
            Arc::clone(&subscription_account_coordinator),
            device_authority,
        ));
        let native_usage = Arc::new(
            NativeUsageService::new(home, Arc::clone(&store))
                .map_err(|_| ControlServerError::State)?,
        );
        activation
            .install_subscription_resolver(device_authorization.clone())
            .map_err(|_| ControlServerError::State)?;
        subscription_account_coordinator
            .recover_pending_intents()
            .await
            .map_err(|_| ControlServerError::State)?;
        if let Some(account_id) = startup_refresh_account_id {
            match device_authorization
                .access_token_for_account(&account_id)
                .await
            {
                Ok(_)
                | Err(
                    crate::subscription::device_authorization::DeviceAuthorizationError::NeedsReauthorization,
                ) => {}
                Err(_) => return Err(ControlServerError::State),
            }
        }
        reconciliation
            .recover_pending_intents()
            .await
            .map_err(|_| ControlServerError::State)?;
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
        let (handover_tx, handover_rx) = mpsc::channel(1);
        let handover = runtime.exit_when_idle.then_some(handover_tx);
        let lifecycle = Arc::new(ServerLifecycle::default());
        let handle_lifecycle = Arc::clone(&lifecycle);
        let task_path = socket_path.clone();
        let handle_reconciliation = Arc::clone(&reconciliation);
        let periodic_usage = Arc::clone(&native_usage);
        let periodic_store = Arc::clone(&store);
        let mut periodic_shutdown = session_shutdown_rx.clone();
        let periodic_usage_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(runtime.native_usage_scan_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                tokio::select! {
                    changed = periodic_shutdown.changed() => {
                        if changed.is_err() || *periodic_shutdown.borrow() {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        for target in [Target::Codex, Target::Claude] {
                            if matches!(periodic_store.committed_takeover_for(target).await, Ok(Some(_)))
                                && periodic_usage.scan(target).await.is_err()
                            {
                                eprintln!("native-usage-scan-failed");
                            }
                        }
                    }
                }
            }
        });
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
                        if runtime.exit_when_idle && should_exit_idle(&store, &lifecycle).await {
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
                        let reconciliation = Arc::clone(&reconciliation);
                        let route_plans = Arc::clone(&route_plans);
                        let provider_synchronization = Arc::clone(&provider_synchronization);
                        let device_authorization = Arc::clone(&device_authorization);
                        let subscription_account_coordinator =
                            Arc::clone(&subscription_account_coordinator);
                        let native_usage = Arc::clone(&native_usage);
                        let handover = handover.clone();
                        let release = release.clone();
                        let lifecycle = Arc::clone(&lifecycle);
                        let session_shutdown = session_shutdown_rx.clone();
                        let services = SessionServices {
                            store,
                            activation,
                            inspector,
                            reconciliation,
                            route_plans,
                            provider_synchronization,
                            device_authorization,
                            subscription_account_coordinator,
                            native_usage,
                            handover,
                        };
                        sessions.spawn(async move {
                            let _guard = SessionGuard(Arc::clone(&lifecycle));
                            serve_authorized(
                                stream,
                                services,
                                release,
                                lifecycle,
                                session_shutdown,
                            ).await;
                        });
                    }
                }
            }
            drop(listener);
            periodic_usage_task.abort();
            let _ = periodic_usage_task.await;
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
            reconciliation: handle_reconciliation,
            handover: runtime.exit_when_idle.then_some(handover_rx),
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

    #[doc(hidden)]
    pub fn tracked_sessions(&self) -> usize {
        self.lifecycle.active_sessions.load(Ordering::Acquire)
    }

    #[doc(hidden)]
    pub fn tracked_writers(&self) -> usize {
        self.lifecycle.active_writers.load(Ordering::Acquire)
    }

    #[doc(hidden)]
    pub async fn tracked_reconciliation_tokens(&self) -> usize {
        self.reconciliation.token_count().await
    }

    #[doc(hidden)]
    pub async fn tracks_reconciliation_token(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        token: uuid::Uuid,
    ) -> bool {
        self.reconciliation
            .tracks_token(target, strategy, token)
            .await
    }

    #[doc(hidden)]
    pub async fn validates_reconciliation_token(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        token: uuid::Uuid,
        claude_context: Option<ClaudePreflightContext>,
    ) -> bool {
        let context = match target {
            Target::Codex => ReconciliationContext::Codex,
            Target::Claude => match claude_context {
                Some(context) => ReconciliationContext::Claude(context),
                None => return false,
            },
        };
        self.reconciliation
            .validate_preview(target, strategy, token, context)
            .await
            .is_ok()
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

    pub(crate) async fn wait_for_lifecycle(
        &mut self,
    ) -> Result<ControlLifecycleOutcome, ControlServerError> {
        let Some(handover) = self.handover.as_mut() else {
            self.wait_for_exit().await?;
            return Ok(ControlLifecycleOutcome::Idle);
        };
        loop {
            tokio::select! {
                biased;
                prepared = handover.recv() => {
                    if let Some(prepared) = prepared {
                        return Ok(ControlLifecycleOutcome::Handover(prepared));
                    }
                    self.wait_for_exit().await?;
                    return Ok(ControlLifecycleOutcome::Idle);
                }
                changed = self.completed.changed() => {
                    changed.map_err(|_| ControlServerError::Task)?;
                    if *self.completed.borrow() {
                        return Ok(ControlLifecycleOutcome::Idle);
                    }
                }
            }
        }
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
    services: SessionServices,
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

    serve_session(stream, services, release, lifecycle, shutdown).await;
}

async fn serve_session(
    mut stream: UnixStream,
    services: SessionServices,
    release: String,
    lifecycle: Arc<ServerLifecycle>,
    mut shutdown: watch::Receiver<bool>,
) {
    let SessionServices {
        store,
        activation,
        inspector,
        reconciliation,
        route_plans,
        provider_synchronization,
        device_authorization,
        subscription_account_coordinator,
        native_usage,
        handover,
    } = services;
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
    let writer_guard = WriterGuard::new(Arc::clone(&lifecycle));
    let mut writer_task = tokio::spawn(write_responses(writer, response_rx, writer_guard));
    let mut opened_target = None;
    let mut opened_universal_providers = false;
    let mut opened_subscription_accounts = false;
    let mut opened_claude_context = None;
    let mut update_rx = store.subscribe_target_views();
    let mut universal_provider_update_rx = store.subscribe_universal_provider_views();
    let mut subscription_account_update_rx = subscription_account_coordinator.subscribe();
    let mut inspections = JoinSet::<InspectionCompletion>::new();
    let mut action_completions = JoinSet::<bool>::new();
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
            completed = action_completions.join_next(), if !action_completions.is_empty() => {
                if !matches!(completed, Some(Ok(true))) {
                    break 'session;
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
                        if let Some(cancellation) = inspection_requests
                            .get(&request_id)
                            .map(|request| request.cancellation.clone())
                        {
                            cancellation.cancel().await;
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

                if let ControlOperation::PrepareHandover(operation) = operation {
                    let prepared = match probe_candidate(
                        PathBuf::from(operation.candidate_path),
                        &operation.expected_release,
                    )
                    .await
                    {
                        Ok(prepared) => prepared,
                        Err(problem) => {
                            if !enqueue_response(
                                &responses,
                                problem_frame(
                                    Some(request_id),
                                    problem.code(),
                                    problem.message(),
                                    None,
                                ),
                            ) {
                                break 'session;
                            }
                            continue;
                        }
                    };
                    let Some(sender) = handover.as_ref() else {
                        if !enqueue_response(
                            &responses,
                            problem_frame(
                                Some(request_id),
                                "unsupported-operation",
                                "Compatible handover requires process mode",
                                None,
                            ),
                        ) {
                            break 'session;
                        }
                        continue;
                    };
                    let Ok(permit) = sender.clone().try_reserve_owned() else {
                        if !enqueue_response(
                            &responses,
                            problem_frame(
                                Some(request_id),
                                "handover-in-progress",
                                "Routing Service handover is already in progress",
                                None,
                            ),
                        ) {
                            break 'session;
                        }
                        continue;
                    };
                    let release = prepared.release.clone();
                    if !enqueue_written_response(
                        &responses,
                        ServerFrame::Response {
                            request_id,
                            result: ControlResult::HandoverPrepared(HandoverPreparedResult {
                                release,
                            }),
                        },
                    )
                    .await
                    {
                        break 'session;
                    }
                    permit.send(prepared);
                    break 'session;
                }

                if matches!(
                    operation,
                    ControlOperation::OpenUniversalProviders { .. }
                        | ControlOperation::UniversalProviderAct { .. }
                ) {
                    if opened_target.is_some() {
                        if !enqueue_response(&responses, problem_frame(
                            Some(request_id),
                            "catalog-session-kind-mismatch",
                            "Universal Provider operations require a catalog session",
                            None,
                        )) {
                            break 'session;
                        }
                        continue;
                    }
                    match operation {
                        ControlOperation::OpenUniversalProviders { claude_context } => {
                            let Ok(view) = store.universal_provider_catalog().await else {
                                if !enqueue_response(&responses, problem_frame(
                                    Some(request_id),
                                    "state-store-error",
                                    "State store unavailable",
                                    None,
                                )) {
                                    break 'session;
                                }
                                continue;
                            };
                            opened_universal_providers = true;
                            opened_claude_context = claude_context;
                            if !enqueue_response(
                                &responses,
                                ServerFrame::Response {
                                    request_id,
                                    result: ControlResult::UniversalProviderCatalog { view },
                                },
                            ) {
                                break 'session;
                            }
                        }
                        ControlOperation::UniversalProviderAct {
                            action_id,
                            expected_revision,
                            action,
                        } => {
                            if !opened_universal_providers {
                                if !enqueue_response(&responses, problem_frame(
                                    Some(request_id),
                                    "universal-provider-catalog-not-open",
                                    "Open the Universal Provider catalog before issuing actions",
                                    None,
                                )) {
                                    break 'session;
                                }
                                continue;
                            }
                            lifecycle.pending_actions.fetch_add(1, Ordering::AcqRel);
                            let _action_guard = ActionGuard(Arc::clone(&lifecycle));
                            let receipt = store.universal_provider_receipt(action_id).await;
                            let (result, catalog_publication, target_publications, eligibility) =
                                match receipt {
                                    Ok(Some(mut outcome)) => {
                                        outcome.status = ActionStatus::Replayed;
                                        (Ok(outcome), None, Vec::new(), None)
                                    }
                                    Ok(None) => {
                                        let parsed_action =
                                            serde_json::from_value::<UniversalProviderAction>(
                                                action.clone(),
                                            );
                                        let synchronize = matches!(
                                            &parsed_action,
                                            Ok(UniversalProviderAction::SynchronizeUniversalProvider { .. })
                                        );
                                        if synchronize {
                                            let attempt = provider_synchronization
                                                .apply_raw(
                                                    action_id,
                                                    expected_revision,
                                                    action,
                                                    opened_claude_context.clone(),
                                                )
                                                .await;
                                            match attempt.result {
                                                Ok(commit) => {
                                                    let catalog = (commit.outcome.status
                                                        == ActionStatus::Applied)
                                                        .then(|| commit.outcome.view.clone());
                                                    (
                                                        Ok(commit.outcome),
                                                        catalog,
                                                        commit.target_views,
                                                        attempt.eligibility_publication,
                                                    )
                                                }
                                                Err(failure) => (
                                                    Err(failure),
                                                    None,
                                                    Vec::new(),
                                                    attempt.eligibility_publication,
                                                ),
                                            }
                                        } else {
                                            let _target_gates = if matches!(
                                                &parsed_action,
                                                Ok(
                                                    UniversalProviderAction::UpdateUniversalProvider { .. }
                                                        | UniversalProviderAction::DeleteUniversalProvider { .. }
                                                )
                                            ) {
                                                Some(
                                                    provider_synchronization
                                                        .lock_catalog_lifecycle_mutation()
                                                        .await,
                                                )
                                            } else {
                                                None
                                            };
                                            let result = store
                                                .apply_universal_provider_action_with_target_views(
                                                    action_id,
                                                    expected_revision,
                                                    action,
                                                )
                                                .await;
                                            match result {
                                                Ok(commit) => {
                                                    let publication = (commit.outcome.status
                                                        == ActionStatus::Applied)
                                                        .then(|| commit.outcome.view.clone());
                                                    (
                                                        Ok(commit.outcome),
                                                        publication,
                                                        commit.target_views,
                                                        None,
                                                    )
                                                }
                                                Err(failure) => {
                                                    (Err(failure), None, Vec::new(), None)
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => (
                                        Err(store
                                            .universal_provider_failure(ControlProblem {
                                                code: "state-store-error".into(),
                                                message: "State store unavailable".into(),
                                                source: None,
                                                selector: None,
                                            })
                                            .await),
                                        None,
                                        Vec::new(),
                                        None,
                                    ),
                                };
                            let frame = match result {
                                Ok(outcome) => ServerFrame::Response {
                                    request_id,
                                    result: ControlResult::UniversalProviderOutcome { outcome },
                                },
                                Err(failure) => ServerFrame::Error {
                                    request_id: Some(request_id),
                                    problem: failure.problem,
                                    authoritative_view: None,
                                    authoritative_universal_provider_view: Some(
                                        failure.authoritative_view,
                                    ),
                                    authoritative_subscription_account_view: None,
                                },
                            };
                            if !enqueue_universal_provider_action_response(
                                &responses,
                                frame,
                                catalog_publication,
                                target_publications,
                                eligibility,
                                &store,
                            )
                            .await
                            {
                                break 'session;
                            }
                        }
                        _ => unreachable!("catalog operation was matched above"),
                    }
                    continue;
                }
                if matches!(operation, ControlOperation::OpenSubscriptionAccounts(_)) {
                    if opened_target.is_some() || opened_universal_providers {
                        if !enqueue_response(&responses, problem_frame(
                            Some(request_id),
                            "catalog-session-kind-mismatch",
                            "Subscription Account operations require an account session",
                            None,
                        )) {
                            break 'session;
                        }
                        continue;
                    }
                    let Ok(view) = subscription_account_coordinator.catalog().await else {
                        if !enqueue_response(&responses, problem_frame(
                            Some(request_id),
                            "state-store-error",
                            "Subscription Account state is unavailable",
                            None,
                        )) {
                            break 'session;
                        }
                        continue;
                    };
                    opened_subscription_accounts = true;
                    if !enqueue_response(&responses, ServerFrame::Response {
                        request_id,
                        result: ControlResult::SubscriptionAccountCatalog { view },
                    }) {
                        break 'session;
                    }
                    continue;
                }
                if matches!(
                    operation,
                    ControlOperation::StartDeviceAuthorization(_)
                        | ControlOperation::PollDeviceAuthorization(_)
                        | ControlOperation::PreviewDefaultSubscriptionAccount(_)
                        | ControlOperation::SubscriptionAccountAct { .. }
                ) {
                    if !opened_subscription_accounts
                        || opened_target.is_some()
                        || opened_universal_providers
                    {
                        if !enqueue_response(&responses, problem_frame(
                            Some(request_id),
                            "catalog-session-kind-mismatch",
                            "Subscription Account operations require an account session",
                            None,
                        )) {
                            break 'session;
                        }
                        continue;
                    }
                    if let ControlOperation::PollDeviceAuthorization(operation) = &operation {
                        if inspection_requests.contains_key(&request_id) {
                            let _ = enqueue_response(
                                &responses,
                                problem_frame(
                                    Some(request_id),
                                    "request-in-progress",
                                    "Request identifier is already in progress",
                                    None,
                                ),
                            );
                            break 'session;
                        }
                        if inspections.len() >= MAX_IN_FLIGHT_INSPECTIONS_PER_SESSION {
                            if !enqueue_response(
                                &responses,
                                problem_frame(
                                    Some(request_id),
                                    "inspection-limit-reached",
                                    "Too many inspections are already in progress",
                                    None,
                                ),
                            ) {
                                break 'session;
                            }
                            continue;
                        }
                        lifecycle
                            .pending_inspections
                            .fetch_add(1, Ordering::AcqRel);
                        let guard = InspectionGuard(Arc::clone(&lifecycle));
                        let cancellation = SubscriptionAuthorizationCancellation::new();
                        let task_request_id = request_id.clone();
                        let abort = inspections.spawn(poll_subscription_account_and_queue(
                            task_request_id,
                            operation.flow_id,
                            Arc::clone(&device_authorization),
                            Arc::clone(&subscription_account_coordinator),
                            responses.clone(),
                            cancellation.clone(),
                            guard,
                        ));
                        inspection_requests.insert(
                            request_id,
                            InspectionRequest {
                                task_id: abort.id(),
                                cancellation: InspectionCancellation::SubscriptionAuthorization(
                                    cancellation,
                                ),
                            },
                        );
                        continue;
                    }
                    let (frame, publication) = match operation {
                        ControlOperation::StartDeviceAuthorization(operation) => {
                            match device_authorization
                                .start(operation.reauthorize_account_id)
                                .await
                            {
                                Ok(challenge) => (
                                    ServerFrame::Response {
                                        request_id,
                                        result: ControlResult::DeviceAuthorizationChallenge {
                                            challenge: crate::control::protocol::DeviceAuthorizationChallengeView {
                                                flow_id: challenge.flow_id,
                                                user_code: challenge.user_code,
                                                verification_url: challenge.verification_url.to_owned(),
                                                expires_in_seconds: challenge.expires_in_seconds,
                                                poll_interval_seconds: challenge.poll_interval_seconds,
                                            },
                                        },
                                    },
                                    None,
                                ),
                                Err(_) => (
                                    problem_frame(
                                        Some(request_id),
                                        "device-authorization-failed",
                                        "Device authorization could not be started",
                                        None,
                                    ),
                                    None,
                                ),
                            }
                        }
                        ControlOperation::PollDeviceAuthorization(_) => {
                            unreachable!("poll operations are spawned as cancellable inspections")
                        }
                        ControlOperation::PreviewDefaultSubscriptionAccount(operation) => {
                            match subscription_account_coordinator
                                .preview_default(&operation.account_id)
                                .await
                            {
                                Ok(preview) => (
                                    ServerFrame::Response {
                                        request_id,
                                        result: ControlResult::SubscriptionDefaultPreview { preview },
                                    },
                                    None,
                                ),
                                Err(failure) => (
                                    ServerFrame::Error {
                                        request_id: Some(request_id),
                                        problem: failure.problem,
                                        authoritative_view: None,
                                        authoritative_universal_provider_view: None,
                                        authoritative_subscription_account_view: Some(Box::new(
                                            failure.authoritative_view,
                                        )),
                                    },
                                    None,
                                ),
                            }
                        }
                        ControlOperation::SubscriptionAccountAct {
                            action_id,
                            expected_revision,
                            action,
                        } => match subscription_account_coordinator
                            .apply_raw(action_id, expected_revision, action)
                            .await
                        {
                                Ok(outcome) => {
                                    let publication = (outcome.status == ActionStatus::Applied)
                                        .then(|| outcome.view.clone());
                                    (
                                        ServerFrame::Response {
                                            request_id,
                                            result: ControlResult::SubscriptionAccountOutcome {
                                                outcome,
                                            },
                                        },
                                        publication,
                                    )
                                }
                                Err(failure) => (
                                    ServerFrame::Error {
                                        request_id: Some(request_id),
                                        problem: failure.problem,
                                        authoritative_view: None,
                                        authoritative_universal_provider_view: None,
                                        authoritative_subscription_account_view: Some(Box::new(
                                            failure.authoritative_view,
                                        )),
                                    },
                                    None,
                                ),
                        },
                        _ => unreachable!("account operation was matched above"),
                    };
                    if !enqueue_subscription_account_response(
                        &responses,
                        frame,
                        publication,
                        &subscription_account_coordinator,
                    )
                    .await
                    {
                        break 'session;
                    }
                    continue;
                }
                let Some(target) = operation_target(&operation) else {
                    unreachable!("all targetless operations are handled before target dispatch")
                };
                if opened_universal_providers {
                    if !enqueue_response(&responses, problem_frame(
                        Some(request_id),
                        "catalog-session-kind-mismatch",
                        "Target operations require a Target session",
                        None,
                    )) {
                        break 'session;
                    }
                    continue;
                }
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
                    ControlOperation::PrepareHandover(_)
                    | ControlOperation::OpenUniversalProviders { .. }
                    | ControlOperation::OpenSubscriptionAccounts(_)
                    | ControlOperation::StartDeviceAuthorization(_)
                    | ControlOperation::PollDeviceAuthorization(_)
                    | ControlOperation::PreviewDefaultSubscriptionAccount(_)
                    | ControlOperation::SubscriptionAccountAct { .. }
                    | ControlOperation::UniversalProviderAct { .. } => {
                        unreachable!("catalog operations are handled before target dispatch")
                    }
                    ControlOperation::OpenTarget { target, claude_context } => {
                        if native_usage.scan(target).await.is_err() {
                            eprintln!("native-usage-scan-failed");
                        }
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
                        let mut action_guard = Some(ActionGuard(Arc::clone(&lifecycle)));
                        let parsed_action =
                            serde_json::from_value::<TargetAction>(action.clone()).ok();
                        let reconcile_action = parsed_action.as_ref().and_then(|action| match action {
                                TargetAction::Reconcile {
                                    strategy,
                                    observation_token,
                                    acknowledge_version,
                                } => Some((*strategy, *observation_token, acknowledge_version.clone())),
                                _ => None,
                            });
                        let compatibility_resolution =
                            parsed_action.as_ref().and_then(|action| match action {
                                TargetAction::ResolveCompatibility(action) => {
                                    Some(action.version.clone())
                                }
                                _ => None,
                            });
                        let disable_takeover = matches!(
                            parsed_action.as_ref(),
                            Some(TargetAction::DisableTakeover(_))
                        );
                        let failover_draft =
                            parsed_action.as_ref().and_then(|action| match action {
                                TargetAction::SaveFailoverDraft(action) => {
                                    Some(action.members.clone())
                                }
                                _ => None,
                            });
                        let failover_apply =
                            parsed_action.as_ref().and_then(|action| match action {
                                TargetAction::ApplyFailoverChain(action) => {
                                    Some(action.draft_revision)
                                }
                                _ => None,
                            });
                        let probe_unmanaged_provider_write = matches!(
                            parsed_action.as_ref(),
                            Some(
                                TargetAction::CreateProvider { .. }
                                    | TargetAction::UpdateProvider { .. }
                                    | TargetAction::ReorderProviders { .. }
                                    | TargetAction::DeleteProvider { .. }
                                    | TargetAction::DuplicateProvider { .. }
                            )
                        );
                        let mut disable_completion = None;
                        let (result, publication) = if disable_takeover {
                            let context = match target {
                                Target::Codex => Some(ReconciliationContext::Codex),
                                Target::Claude => opened_claude_context
                                    .clone()
                                    .map(ReconciliationContext::Claude),
                            };
                            match context {
                                Some(context) => {
                                    let deferred = reconciliation
                                        .disable_takeover(
                                            target,
                                            action_id,
                                            expected_revision,
                                            context,
                                        )
                                        .await;
                                    disable_completion = deferred.completion;
                                    (deferred.result, deferred.publication)
                                }
                                None => (Err(store.failure_for(
                                    target,
                                    "preflight-context-required",
                                    "Claude preflight context is required",
                                ).await), None),
                            }
                        } else if let Some((strategy, observation_token, acknowledge_version)) = reconcile_action {
                            match store.receipt_for(target, action_id).await {
                                Ok(Some(outcome)) => (Ok(outcome), None),
                                Ok(None) => {
                                    let context = match target {
                                        Target::Codex => Some(ReconciliationContext::Codex),
                                        Target::Claude => opened_claude_context
                                            .clone()
                                            .map(ReconciliationContext::Claude),
                                    };
                                    match context {
                                        Some(context) => {
                                            let deferred = reconciliation.apply(
                                                target,
                                                action_id,
                                                expected_revision,
                                                strategy,
                                                observation_token,
                                                acknowledge_version,
                                                context,
                                            )
                                            .await;
                                            (deferred.result, deferred.publication)
                                        }
                                        None => (Err(store.failure_for(
                                            target,
                                            "preflight-context-required",
                                            "Claude preflight context is required",
                                        ).await), None),
                                    }
                                }
                                Err(_) => (Err(store.failure_for(
                                    target,
                                    "state-store-error",
                                    "State store unavailable",
                                ).await), None),
                            }
                        } else if let Some(version) = compatibility_resolution {
                            let deferred = reconciliation
                                .resolve_compatibility(
                                    target,
                                    action_id,
                                    expected_revision,
                                    version,
                                )
                                .await;
                            (deferred.result, deferred.publication)
                        } else if let Some(members) = failover_draft {
                            let deferred = route_plans
                                .save_draft(target, action_id, expected_revision, members)
                                .await;
                            (deferred.result, deferred.publication)
                        } else if let Some(draft_revision) = failover_apply {
                            let context = match target {
                                Target::Codex => Some(ReconciliationContext::Codex),
                                Target::Claude => opened_claude_context
                                    .clone()
                                    .map(ReconciliationContext::Claude),
                            };
                            let deferred = route_plans
                                .apply(
                                    target,
                                    action_id,
                                    expected_revision,
                                    draft_revision,
                                    context,
                                )
                                .await;
                            (deferred.result, deferred.publication)
                        } else {
                            match store.receipt_for(target, action_id).await {
                                Ok(Some(outcome)) => (Ok(outcome), None),
                                Ok(None) => {
                                    let context = match target {
                                        Target::Codex => Some(ReconciliationContext::Codex),
                                        Target::Claude => opened_claude_context
                                            .clone()
                                            .map(ReconciliationContext::Claude),
                                    };
                                    let _gate = reconciliation
                                        .lock_target_mutation(target)
                                        .await;
                                    let allowed = reconciliation
                                        .ensure_ordinary_write_allowed(
                                            target,
                                            context,
                                            probe_unmanaged_provider_write,
                                        )
                                        .await;
                                    match allowed.result {
                                        Ok(()) => {
                                            let result = activation
                                                .apply_raw_for_with_context_already_held(
                                                target,
                                                action_id,
                                                expected_revision,
                                                action,
                                                opened_claude_context.as_ref(),
                                            )
                                            .await;
                                            let publication = result.as_ref().err().and_then(|failure| {
                                                matches!(
                                                    failure.problem.code.as_str(),
                                                    "compatibility-acknowledgement-required"
                                                        | "incompatible-target-cli"
                                                )
                                                .then(|| failure.authoritative_view.clone())
                                            });
                                            (result, publication)
                                        }
                                        Err(failure) => (Err(failure), allowed.publication),
                                    }
                                }
                                Err(_) => (Err(store
                                    .failure_for(
                                        target,
                                        "state-store-error",
                                        "State store unavailable",
                                    )
                                    .await), None),
                            }
                        };
                        let frame = match result {
                            Ok(outcome) => {
                                ServerFrame::Response {
                                    request_id,
                                    result: ControlResult::ActionOutcome { outcome },
                                }
                            }
                            Err(failure) => {
                                ServerFrame::Error {
                                    request_id: Some(request_id),
                                    problem: failure.problem,
                                    authoritative_view: Some(failure.authoritative_view),
                                    authoritative_universal_provider_view: None,
                                    authoritative_subscription_account_view: None,
                                }
                            }
                        };
                        let response_delivered =
                            enqueue_action_response(&responses, frame, publication, &store).await;
                        if let Some(completion) = disable_completion {
                            let reconciliation = Arc::clone(&reconciliation);
                            let store = Arc::clone(&store);
                            let guard = action_guard
                                .take()
                                .expect("every action owns one lifecycle guard");
                            action_completions.spawn(async move {
                                let _guard = guard;
                                match reconciliation.complete_disable_takeover(completion).await {
                                    Ok(Some(recovery_view)) => {
                                        store.publish_target_view(recovery_view).await.is_ok()
                                    }
                                    Ok(None) => true,
                                    Err(_) => false,
                                }
                            });
                        }
                        if !response_delivered {
                            break 'session;
                        }
                    }
                    operation @ (ControlOperation::DiscoverModels { .. }
                    | ControlOperation::CheckReachability { .. }
                    | ControlOperation::PreviewReconciliation { .. }
                    | ControlOperation::ProbeCompatibility(_)
                    | ControlOperation::ListRequestRecords(_)
                    | ControlOperation::InspectRequestRecord(_)
                    | ControlOperation::ListUsageActivity(_)
                    | ControlOperation::RefreshNativeUsage(_)
                    | ControlOperation::SetUsageRetention(_)
                    | ControlOperation::ClearUsage(_)
                    | ControlOperation::UpdatePricingCatalog(_)) => {
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
                        let inspection_services = InspectionServices {
                            inspector: Arc::clone(&inspector),
                            reconciliation: Arc::clone(&reconciliation),
                            store: Arc::clone(&store),
                            native_usage: Arc::clone(&native_usage),
                        };
                        let claude_context = opened_claude_context.clone();
                        let responses = responses.clone();
                        let task_request_id = request_id.clone();
                        let cancellation = CancellationToken::new();
                        let abort = inspections.spawn(inspect_and_queue(
                            task_request_id,
                            inspection_services,
                            InspectionOperation {
                                target,
                                operation,
                                opened_claude_context: claude_context,
                            },
                            responses,
                            cancellation.clone(),
                            guard,
                        ));
                        inspection_requests.insert(request_id, InspectionRequest {
                            task_id: abort.id(),
                            cancellation: InspectionCancellation::Standard(cancellation),
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
            update = universal_provider_update_rx.recv(), if opened_universal_providers => {
                match update {
                    Ok(view) => {
                        if !enqueue_response(&responses, ServerFrame::UniversalProviderView { view }) {
                            break 'session;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Ok(view) = store.universal_provider_catalog().await else { continue };
                        if !enqueue_response(&responses, ServerFrame::UniversalProviderView { view }) {
                            break 'session;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break 'session,
                }
            }
            update = subscription_account_update_rx.recv(), if opened_subscription_accounts => {
                match update {
                    Ok(view) => {
                        if !enqueue_response(&responses, ServerFrame::SubscriptionAccountView { view }) {
                            break 'session;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Ok(view) = subscription_account_coordinator.catalog().await else { continue };
                        if !enqueue_response(&responses, ServerFrame::SubscriptionAccountView { view }) {
                            break 'session;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break 'session,
                }
            }
        }
    }

    let cancellations = inspection_requests
        .values()
        .map(|request| request.cancellation.clone())
        .collect::<Vec<_>>();
    for cancellation in cancellations {
        cancellation.cancel().await;
    }
    inspection_requests.clear();
    while action_completions.join_next().await.is_some() {}
    drop(responses);
    let inspections_drained = tokio::time::timeout(Duration::from_millis(250), async {
        while inspections.join_next().await.is_some() {}
    })
    .await
    .is_ok();
    if !inspections_drained {
        writer_task.abort();
        let _ = writer_task.await;
        while inspections.join_next().await.is_some() {}
        return;
    }
    if tokio::time::timeout(Duration::from_millis(250), &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
        let _ = writer_task.await;
    }
}

fn operation_target(operation: &ControlOperation) -> Option<Target> {
    match operation {
        ControlOperation::PrepareHandover(_)
        | ControlOperation::OpenUniversalProviders { .. }
        | ControlOperation::OpenSubscriptionAccounts(_)
        | ControlOperation::StartDeviceAuthorization(_)
        | ControlOperation::PollDeviceAuthorization(_)
        | ControlOperation::PreviewDefaultSubscriptionAccount(_)
        | ControlOperation::SubscriptionAccountAct { .. }
        | ControlOperation::UniversalProviderAct { .. } => None,
        ControlOperation::OpenTarget { target, .. }
        | ControlOperation::Act { target, .. }
        | ControlOperation::DiscoverModels { target, .. }
        | ControlOperation::CheckReachability { target, .. }
        | ControlOperation::PreviewReconciliation { target, .. } => Some(*target),
        ControlOperation::ProbeCompatibility(operation) => Some(operation.target),
        ControlOperation::ListRequestRecords(operation) => Some(operation.target),
        ControlOperation::InspectRequestRecord(operation) => Some(operation.target),
        ControlOperation::ListUsageActivity(operation) => Some(operation.target),
        ControlOperation::RefreshNativeUsage(operation)
        | ControlOperation::ClearUsage(operation)
        | ControlOperation::UpdatePricingCatalog(operation) => Some(operation.target),
        ControlOperation::SetUsageRetention(operation) => Some(operation.target),
    }
}

async fn poll_subscription_account_and_queue(
    request_id: String,
    flow_id: Uuid,
    device_authorization: Arc<DeviceAuthorizationManager>,
    coordinator: Arc<SubscriptionAccountCoordinator>,
    responses: mpsc::Sender<QueuedResponse>,
    cancellation: SubscriptionAuthorizationCancellation,
    guard: InspectionGuard,
) -> InspectionCompletion {
    let _guard = guard;
    let cancellation_token = cancellation.token();
    let polled = tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => {
            return InspectionCompletion {
                request_id,
                disposition: InspectionDisposition::Cancelled,
            };
        }
        result = device_authorization.poll(flow_id) => result,
    };
    if cancellation_token.is_cancelled() {
        return InspectionCompletion {
            request_id,
            disposition: InspectionDisposition::Cancelled,
        };
    }
    let (frame, publication) = match polled {
        Ok(crate::subscription::DeviceAuthorizationPoll::Pending) => (
            ServerFrame::Response {
                request_id: request_id.clone(),
                result: ControlResult::DeviceAuthorizationPoll {
                    poll: crate::control::protocol::DeviceAuthorizationPollView::Pending,
                },
            },
            None,
        ),
        Ok(crate::subscription::DeviceAuthorizationPoll::Expired) => (
            ServerFrame::Response {
                request_id: request_id.clone(),
                result: ControlResult::DeviceAuthorizationPoll {
                    poll: crate::control::protocol::DeviceAuthorizationPollView::Expired,
                },
            },
            None,
        ),
        Ok(crate::subscription::DeviceAuthorizationPoll::Authorized { authorization }) => {
            let account = authorization.account.clone();
            match coordinator
                .record_authorization_cancellable(flow_id, account, &cancellation)
                .await
            {
                Ok(SubscriptionAuthorizationCommit::Cancelled) => {
                    return InspectionCompletion {
                        request_id,
                        disposition: InspectionDisposition::Cancelled,
                    };
                }
                Ok(SubscriptionAuthorizationCommit::Committed(publication)) => match device_authorization
                    .complete_authorization(flow_id, authorization)
                    .await
                {
                    Ok(account_id) => (
                        ServerFrame::Response {
                            request_id: request_id.clone(),
                            result: ControlResult::DeviceAuthorizationPoll {
                                poll: crate::control::protocol::DeviceAuthorizationPollView::Authorized {
                                    account_id,
                                },
                            },
                        },
                        Some(publication),
                    ),
                    Err(_) => (
                        problem_frame(
                            Some(request_id.clone()),
                            "device-authorization-failed",
                            "Device authorization could not be finalized",
                            None,
                        ),
                        None,
                    ),
                },
                Err(failure) => (
                    ServerFrame::Error {
                        request_id: Some(request_id.clone()),
                        problem: failure.problem,
                        authoritative_view: None,
                        authoritative_universal_provider_view: None,
                        authoritative_subscription_account_view: Some(Box::new(
                            failure.authoritative_view,
                        )),
                    },
                    None,
                ),
            }
        }
        Err(_) => (
            problem_frame(
                Some(request_id.clone()),
                "device-authorization-failed",
                "Device authorization could not be polled",
                None,
            ),
            None,
        ),
    };
    let disposition = if enqueue_subscription_account_response(
        &responses,
        frame,
        publication,
        &coordinator,
    )
    .await
    {
        InspectionDisposition::Written
    } else {
        InspectionDisposition::CloseSession
    };
    InspectionCompletion {
        request_id,
        disposition,
    }
}

async fn inspect_and_queue(
    request_id: String,
    services: InspectionServices,
    work: InspectionOperation,
    responses: mpsc::Sender<QueuedResponse>,
    cancellation: CancellationToken,
    guard: InspectionGuard,
) -> InspectionCompletion {
    let _guard = guard;
    let InspectionServices {
        inspector,
        reconciliation,
        store,
        native_usage,
    } = services;
    let InspectionOperation {
        target,
        operation,
        opened_claude_context,
    } = work;
    let is_reconciliation_preview =
        matches!(&operation, ControlOperation::PreviewReconciliation { .. });
    let inspection_cancellation = cancellation.clone();
    let inspection = async {
        match operation {
            ControlOperation::DiscoverModels { source, .. } => Ok((
                ControlResult::ModelDiscovery {
                    result: inspector.discover_models_for(target, source).await,
                },
                None,
            )),
            ControlOperation::CheckReachability {
                provider_id,
                provider_revision,
                ..
            } => Ok((
                ControlResult::Reachability {
                    result: inspector
                        .check_reachability_for(target, provider_id, provider_revision)
                        .await,
                },
                None,
            )),
            ControlOperation::PreviewReconciliation {
                strategy,
                claude_context,
                ..
            } => {
                let context = match target {
                    Target::Codex => ReconciliationContext::Codex,
                    Target::Claude => ReconciliationContext::Claude(
                        claude_context
                            .or(opened_claude_context)
                            .ok_or_else(|| ControlProblem {
                                code: "preflight-context-required".into(),
                                message: "Claude preflight context is required".into(),
                                source: None,
                                selector: None,
                            })?,
                    ),
                };
                reconciliation
                    .preview_registered_cancellable(
                        target,
                        strategy,
                        context,
                        inspection_cancellation,
                    )
                    .await
                    .map(|registration| {
                        (
                            ControlResult::ReconciliationPreview {
                                preview: registration.preview.clone(),
                            },
                            Some(registration),
                        )
                    })
            }
            ControlOperation::ProbeCompatibility(_) => reconciliation
                .probe_compatibility_cancellable(target, inspection_cancellation)
                .await
                .map(|probe| {
                    (
                        ControlResult::CompatibilityProbe(CompatibilityProbeResult { probe }),
                        None,
                    )
                }),
            ControlOperation::ListRequestRecords(operation) => store
                .list_request_records(
                    operation.target,
                    operation.before_cursor.as_deref(),
                    operation.limit,
                )
                .await
                .map(|page| {
                    (
                        ControlResult::RequestRecordPage(
                            crate::control::protocol::RequestRecordPageResult { page },
                        ),
                        None,
                    )
                })
                .map_err(request_history_problem),
            ControlOperation::InspectRequestRecord(operation) => store
                .inspect_request_record(operation.target, operation.record_id)
                .await
                .map(|detail| {
                    (
                        ControlResult::RequestRecordDetail(
                            crate::control::protocol::RequestRecordDetailResult { detail },
                        ),
                        None,
                    )
                })
                .map_err(request_history_problem),
            ControlOperation::ListUsageActivity(operation) => native_usage
                .list(
                    operation.target,
                    operation.before_cursor.as_deref(),
                    operation.limit,
                )
                .await
                .map(|page| {
                    (
                        ControlResult::UsageActivityPage(
                            crate::control::protocol::UsageActivityPageResult { page },
                        ),
                        None,
                    )
                })
                .map_err(native_usage_problem),
            ControlOperation::RefreshNativeUsage(operation) => native_usage
                .scan(operation.target)
                .await
                .map(|refresh| {
                    (
                        ControlResult::NativeUsageRefresh(
                            crate::control::protocol::NativeUsageRefreshResult { refresh },
                        ),
                        None,
                    )
                })
                .map_err(native_usage_problem),
            ControlOperation::SetUsageRetention(operation) => native_usage
                .set_retention(operation.target, operation.detailed_retention_days)
                .await
                .map(|outcome| {
                    (
                        ControlResult::UsageRetentionOutcome(
                            crate::control::protocol::UsageRetentionOutcomeResult { outcome },
                        ),
                        None,
                    )
                })
                .map_err(native_usage_problem),
            ControlOperation::ClearUsage(operation) => native_usage
                .clear(operation.target)
                .await
                .map(|outcome| {
                    (
                        ControlResult::UsageClearOutcome(
                            crate::control::protocol::UsageClearOutcomeResult { outcome },
                        ),
                        None,
                    )
                })
                .map_err(native_usage_problem),
            ControlOperation::UpdatePricingCatalog(operation) => native_usage
                .update_catalog(operation.target)
                .await
                .map(|outcome| {
                    (
                        ControlResult::PricingCatalogUpdateOutcome(
                            crate::control::protocol::PricingCatalogUpdateOutcomeResult { outcome },
                        ),
                        None,
                    )
                })
                .map_err(native_usage_problem),
            _ => unreachable!(),
        }
    };
    let result = if is_reconciliation_preview {
        let result = inspection.await;
        if cancellation.is_cancelled() {
            if let Ok((_, Some(registration))) = result {
                reconciliation.rollback_preview(registration).await;
            }
            return InspectionCompletion {
                request_id,
                disposition: InspectionDisposition::Cancelled,
            };
        }
        result
    } else {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return InspectionCompletion {
                    request_id,
                    disposition: InspectionDisposition::Cancelled,
                };
            }
            result = inspection => result,
        }
    };

    let (written, write_acknowledged) = oneshot::channel();
    let (frame, registration) = match result {
        Ok((result, registration)) => (
            ServerFrame::Response {
                request_id: request_id.clone(),
                result,
            },
            registration,
        ),
        Err(problem) => (
            ServerFrame::Error {
                request_id: Some(request_id.clone()),
                problem,
                authoritative_view: None,
                authoritative_universal_provider_view: None,
                authoritative_subscription_account_view: None,
            },
            None,
        ),
    };
    if responses
        .try_send(QueuedResponse {
            frame,
            written: Some(written),
        })
        .is_err()
    {
        if let Some(registration) = registration {
            reconciliation.rollback_preview(registration).await;
        }
        return InspectionCompletion {
            request_id,
            disposition: InspectionDisposition::CloseSession,
        };
    }
    let disposition = if write_acknowledged.await.is_ok() {
        InspectionDisposition::Written
    } else {
        if let Some(registration) = registration {
            reconciliation.rollback_preview(registration).await;
        }
        InspectionDisposition::CloseSession
    };
    InspectionCompletion {
        request_id,
        disposition,
    }
}

fn request_history_problem(error: crate::request_history::RequestHistoryError) -> ControlProblem {
    let (code, message) = match error {
        crate::request_history::RequestHistoryError::Unavailable => (
            "request-history-unavailable",
            "Request history is unavailable",
        ),
        crate::request_history::RequestHistoryError::InvalidCursor => (
            "invalid-request-history-cursor",
            "Request history cursor is invalid",
        ),
        crate::request_history::RequestHistoryError::NotFound => {
            ("request-record-not-found", "Request record was not found")
        }
    };
    ControlProblem {
        code: code.into(),
        message: message.into(),
        source: None,
        selector: None,
    }
}

fn native_usage_problem(error: NativeUsageError) -> ControlProblem {
    let (code, message) = match error {
        NativeUsageError::Unavailable => {
            ("native-usage-unavailable", "Native usage is unavailable")
        }
        NativeUsageError::InvalidRequest => ("invalid-usage-request", "Usage request is invalid"),
        NativeUsageError::InvalidCatalog => (
            "invalid-pricing-catalog",
            "Pricing catalog candidate is invalid",
        ),
    };
    ControlProblem {
        code: code.into(),
        message: message.into(),
        source: None,
        selector: None,
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

async fn enqueue_action_response(
    responses: &mpsc::Sender<QueuedResponse>,
    frame: ServerFrame,
    publication: Option<TargetView>,
    store: &StateStore,
) -> bool {
    let (written, acknowledged) = oneshot::channel();
    if responses
        .try_send(QueuedResponse {
            frame,
            written: Some(written),
        })
        .is_err()
    {
        return false;
    }
    if acknowledged.await.is_err() {
        return false;
    }
    if let Some(view) = publication
        && store.publish_target_view(view).await.is_err()
    {
        return false;
    }
    let Ok(catalog) = store.universal_provider_catalog().await else {
        return false;
    };
    if store
        .publish_universal_provider_view(catalog)
        .await
        .is_err()
    {
        return false;
    }
    true
}

async fn enqueue_written_response(
    responses: &mpsc::Sender<QueuedResponse>,
    frame: ServerFrame,
) -> bool {
    let (written, acknowledged) = oneshot::channel();
    if responses
        .try_send(QueuedResponse {
            frame,
            written: Some(written),
        })
        .is_err()
    {
        return false;
    }
    acknowledged.await.is_ok()
}

async fn enqueue_subscription_account_response(
    responses: &mpsc::Sender<QueuedResponse>,
    frame: ServerFrame,
    publication: Option<crate::control::protocol::SubscriptionAccountCatalogView>,
    coordinator: &SubscriptionAccountCoordinator,
) -> bool {
    let (written, acknowledged) = oneshot::channel();
    if responses
        .try_send(QueuedResponse {
            frame,
            written: Some(written),
        })
        .is_err()
    {
        return false;
    }
    if acknowledged.await.is_err() {
        return false;
    }
    if let Some(view) = publication {
        coordinator.publish(view).await;
    }
    true
}

async fn enqueue_universal_provider_action_response(
    responses: &mpsc::Sender<QueuedResponse>,
    frame: ServerFrame,
    catalog_publication: Option<UniversalProviderCatalogView>,
    target_publications: Vec<TargetView>,
    eligibility_publication: Option<TargetView>,
    store: &StateStore,
) -> bool {
    let (written, acknowledged) = oneshot::channel();
    if responses
        .try_send(QueuedResponse {
            frame,
            written: Some(written),
        })
        .is_err()
    {
        return false;
    }
    if acknowledged.await.is_err() {
        return false;
    }
    if let Some(view) = catalog_publication
        && store.publish_universal_provider_view(view).await.is_err()
    {
        return false;
    }
    for view in target_publications
        .into_iter()
        .chain(eligibility_publication)
    {
        if store.publish_target_view(view).await.is_err() {
            return false;
        }
    }
    true
}

async fn write_responses(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut responses: mpsc::Receiver<QueuedResponse>,
    writer_guard: WriterGuard,
) {
    while let Some(response) = responses.recv().await {
        let QueuedResponse { frame, written } = response;
        if !matches!(
            tokio::time::timeout(FRAME_WRITE_TIMEOUT, write_frame(&mut writer, &frame)).await,
            Ok(Ok(_))
        ) {
            drop(writer);
            writer_guard.closed();
            drop(written);
            drop(responses);
            return;
        }
        if let Some(written) = written {
            let _ = written.send(());
        }
    }
    drop(writer);
    writer_guard.closed();
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
        authoritative_universal_provider_view: None,
        authoritative_subscription_account_view: None,
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
            authoritative_universal_provider_view: None,
            authoritative_subscription_account_view: None,
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
