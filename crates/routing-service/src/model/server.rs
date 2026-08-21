use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Method, Request, Response, StatusCode},
    routing::post,
};
use futures_util::StreamExt;
use reqwest::Body as ReqwestBody;
use secrecy::ExposeSecret;
use tokio::{
    net::TcpListener,
    sync::{Notify, oneshot},
    task::JoinHandle,
};

use crate::control::protocol::Target;
use crate::state::StateStore;
use crate::subscription::resolver::SubscriptionAccountResolver;

use super::messages::route_messages;
use super::{
    auth::routing_credential_matches,
    headers::{forward_request_headers, forward_response_headers},
    request_recorder::{RequestRecorder, ResponseUsageFormat, recorded_body},
    router::{
        PreparedRouteAttempt, RouteAttemptFailure, RouteHealthRuntime, RouteResponseKind,
        pin_route_plan, route_pinned_plan,
    },
    upstream::{UpstreamRequest, UpstreamTransport, responses_url},
};

#[derive(Debug, thiserror::Error)]
pub enum ModelServerError {
    #[error("model listener must be bound to IPv4 localhost")]
    NonLoopbackListener,
    #[error("model server I/O failed")]
    Io(#[from] io::Error),
    #[error("model server task failed")]
    Task,
    #[error("model server state is unavailable")]
    State,
    #[error("target committed route state is inconsistent")]
    TargetState,
    #[error("target configuration home is unavailable")]
    TargetConfiguration,
}

pub struct ReservedListener {
    listener: TcpListener,
    endpoint: SocketAddr,
}

impl ReservedListener {
    pub fn new(listener: TcpListener) -> Result<Self, ModelServerError> {
        let endpoint = listener.local_addr()?;
        if endpoint.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return Err(ModelServerError::NonLoopbackListener);
        }
        Ok(Self { listener, endpoint })
    }

    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }
}

pub struct ModelServer;

const RESERVED_BIT: usize = 1;
const ACTIVE_REQUEST_INCREMENT: usize = 2;
const MAX_REPLAYABLE_BODY_BYTES: usize = 32 * 1024 * 1024;

pub struct ModelServerHandle {
    endpoint: SocketAddr,
    activate: Option<oneshot::Sender<()>>,
    running: Option<oneshot::Receiver<()>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), io::Error>>>,
    status: Arc<AtomicU8>,
    shutdown_requested: Arc<AtomicBool>,
    admission: Arc<ModelAdmission>,
    #[cfg(test)]
    reservation_attempt_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub(crate) struct ModelAdmission {
    state: AtomicUsize,
    drained: Notify,
}

impl ModelAdmission {
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            drained: Notify::new(),
        }
    }

    fn active_request_count(&self) -> usize {
        self.state.load(Ordering::Acquire) / ACTIVE_REQUEST_INCREMENT
    }

    pub(crate) fn rejects_new_requests(&self) -> bool {
        self.state.load(Ordering::Acquire) & RESERVED_BIT != 0
    }

    fn try_reserve_idle(&self) -> bool {
        self.state
            .compare_exchange(0, RESERVED_BIT, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn begin_draining(&self) -> bool {
        let mut observed = self.state.load(Ordering::Acquire);
        loop {
            if observed & RESERVED_BIT != 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                observed,
                observed | RESERVED_BIT,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => observed = actual,
            }
        }
    }

    fn resume(&self) {
        let mut observed = self.state.load(Ordering::Acquire);
        loop {
            debug_assert_ne!(observed & RESERVED_BIT, 0);
            match self.state.compare_exchange_weak(
                observed,
                observed & !RESERVED_BIT,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => observed = actual,
            }
        }
    }

    async fn wait_until_drained(&self) {
        loop {
            let drained = self.drained.notified();
            if self.state.load(Ordering::Acquire) == RESERVED_BIT {
                return;
            }
            drained.await;
        }
    }
}

pub struct ModelDrainReservation {
    pub(crate) admission: Arc<ModelAdmission>,
    committed: bool,
}

impl ModelDrainReservation {
    pub async fn wait_until_drained(&self) {
        self.admission.wait_until_drained().await;
    }

    pub fn commit(mut self) -> bool {
        if self.admission.state.load(Ordering::Acquire) != RESERVED_BIT {
            return false;
        }
        self.committed = true;
        true
    }
}

impl Drop for ModelDrainReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.admission.resume();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelServerStatus {
    Starting,
    Running,
    Stopped,
    Failed,
}

impl ModelServerStatus {
    fn encode(self) -> u8 {
        match self {
            Self::Starting => 0,
            Self::Running => 1,
            Self::Stopped => 2,
            Self::Failed => 3,
        }
    }

    fn decode(value: u8) -> Self {
        match value {
            0 => Self::Starting,
            1 => Self::Running,
            2 => Self::Stopped,
            _ => Self::Failed,
        }
    }
}

pub(crate) struct RouteState {
    pub(crate) target: Target,
    pub(crate) store: Arc<StateStore>,
    pub(crate) upstream: Arc<dyn UpstreamTransport>,
    pub(crate) admission: Arc<ModelAdmission>,
    pub(crate) route_health: Arc<RouteHealthRuntime>,
    pub(crate) subscription_resolver: Option<Arc<dyn SubscriptionAccountResolver>>,
    pub(crate) request_recorder: RequestRecorder,
}

impl Clone for RouteState {
    fn clone(&self) -> Self {
        Self {
            target: self.target,
            store: Arc::clone(&self.store),
            upstream: Arc::clone(&self.upstream),
            admission: Arc::clone(&self.admission),
            route_health: Arc::clone(&self.route_health),
            subscription_resolver: self.subscription_resolver.clone(),
            request_recorder: self.request_recorder.clone(),
        }
    }
}

impl ModelServer {
    pub async fn bind_reserved(
        reserved: ReservedListener,
        store: Arc<StateStore>,
        upstream: Arc<dyn UpstreamTransport>,
    ) -> Result<ModelServerHandle, ModelServerError> {
        Self::bind_reserved_for(reserved, Target::Codex, store, upstream).await
    }

    pub async fn bind_reserved_for(
        reserved: ReservedListener,
        target: Target,
        store: Arc<StateStore>,
        upstream: Arc<dyn UpstreamTransport>,
    ) -> Result<ModelServerHandle, ModelServerError> {
        let handle = Self::bind_reserved_for_with_health(
            reserved,
            target,
            store,
            upstream,
            Arc::new(RouteHealthRuntime::default()),
            None,
        )
        .await?;
        Ok(handle)
    }

    pub(crate) async fn bind_reserved_for_with_health(
        reserved: ReservedListener,
        target: Target,
        store: Arc<StateStore>,
        upstream: Arc<dyn UpstreamTransport>,
        route_health: Arc<RouteHealthRuntime>,
        subscription_resolver: Option<Arc<dyn SubscriptionAccountResolver>>,
    ) -> Result<ModelServerHandle, ModelServerError> {
        let mut handle = Self::bind_reserved_staged_for_with_health(
            reserved,
            target,
            store,
            upstream,
            route_health,
            subscription_resolver,
        )
        .await?;
        handle.activate().await?;
        Ok(handle)
    }

    pub async fn bind_reserved_staged_for(
        reserved: ReservedListener,
        target: Target,
        store: Arc<StateStore>,
        upstream: Arc<dyn UpstreamTransport>,
    ) -> Result<ModelServerHandle, ModelServerError> {
        Self::bind_reserved_staged_for_with_health(
            reserved,
            target,
            store,
            upstream,
            Arc::new(RouteHealthRuntime::default()),
            None,
        )
        .await
    }

    pub(crate) async fn bind_reserved_staged_for_with_health(
        reserved: ReservedListener,
        target: Target,
        store: Arc<StateStore>,
        upstream: Arc<dyn UpstreamTransport>,
        route_health: Arc<RouteHealthRuntime>,
        subscription_resolver: Option<Arc<dyn SubscriptionAccountResolver>>,
    ) -> Result<ModelServerHandle, ModelServerError> {
        let endpoint = reserved.endpoint;
        let (request_recorder, request_recorder_actor) =
            RequestRecorder::new(Arc::clone(&store)).map_err(|_| ModelServerError::State)?;
        let state = RouteState {
            target,
            store,
            upstream,
            admission: Arc::new(ModelAdmission::new()),
            route_health,
            subscription_resolver,
            request_recorder,
        };
        let admission = Arc::clone(&state.admission);
        let router = match target {
            Target::Codex => Router::new().route("/v1/responses", post(route_responses)),
            Target::Claude => Router::new()
                .route("/v1/messages", post(route_messages))
                .route("/v1/messages/count_tokens", post(route_messages)),
        }
        .with_state(state);
        let (activate, activate_rx) = oneshot::channel();
        let (running_tx, running_rx) = oneshot::channel();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (staged_tx, staged_rx) = oneshot::channel();
        let status = Arc::new(AtomicU8::new(ModelServerStatus::Starting.encode()));
        let task_status = Arc::clone(&status);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let task_shutdown_requested = Arc::clone(&shutdown_requested);
        let task = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_rx;
            let _ = staged_tx.send(());
            let activated = tokio::select! {
                activated = activate_rx => activated.is_ok(),
                _ = &mut shutdown_rx => false,
            };
            if !activated {
                task_status.store(ModelServerStatus::Stopped.encode(), Ordering::Release);
                return Ok(());
            }
            task_status.store(ModelServerStatus::Running.encode(), Ordering::Release);
            let _ = running_tx.send(());
            let request_recorder_task = tokio::spawn(request_recorder_actor.run());
            let result = axum::serve(reserved.listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
            if request_recorder_task.await.is_err() {
                task_status.store(ModelServerStatus::Failed.encode(), Ordering::Release);
                return Err(io::Error::other("request recorder task failed"));
            }
            let final_status = if result.is_ok() && task_shutdown_requested.load(Ordering::Acquire)
            {
                ModelServerStatus::Stopped
            } else {
                ModelServerStatus::Failed
            };
            task_status.store(final_status.encode(), Ordering::Release);
            result
        });
        if staged_rx.await.is_err() {
            status.store(ModelServerStatus::Failed.encode(), Ordering::Release);
            return Err(ModelServerError::Task);
        }
        let handle = ModelServerHandle {
            endpoint,
            activate: Some(activate),
            running: Some(running_rx),
            shutdown: Some(shutdown),
            task: Some(task),
            status,
            shutdown_requested,
            admission,
            #[cfg(test)]
            reservation_attempt_hook: None,
        };
        if handle.task.as_ref().is_none_or(JoinHandle::is_finished) {
            return Err(ModelServerError::Task);
        }
        Ok(handle)
    }
}

impl ModelServerHandle {
    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    pub fn status(&self) -> ModelServerStatus {
        let status = ModelServerStatus::decode(self.status.load(Ordering::Acquire));
        if status == ModelServerStatus::Running
            && self.task.as_ref().is_none_or(JoinHandle::is_finished)
        {
            ModelServerStatus::Failed
        } else {
            status
        }
    }

    pub fn is_running(&self) -> bool {
        self.status() == ModelServerStatus::Running
    }

    pub fn is_staged(&self) -> bool {
        self.status() == ModelServerStatus::Starting
            && self.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    pub fn active_request_count(&self) -> usize {
        self.admission.active_request_count()
    }

    pub fn begin_draining(&self) -> Option<ModelDrainReservation> {
        self.admission
            .begin_draining()
            .then(|| ModelDrainReservation {
                admission: Arc::clone(&self.admission),
                committed: false,
            })
    }

    pub(crate) fn try_reserve_idle(&self) -> bool {
        let reserved = self.admission.try_reserve_idle();
        #[cfg(test)]
        if let Some(hook) = &self.reservation_attempt_hook {
            hook();
        }
        reserved
    }

    pub(crate) fn release_idle_reservation(&self) {
        debug_assert_eq!(self.admission.state.load(Ordering::Acquire), RESERVED_BIT);
        self.admission.resume();
    }

    #[cfg(test)]
    pub(crate) fn set_reservation_attempt_hook(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.reservation_attempt_hook = Some(hook);
    }

    pub async fn activate(&mut self) -> Result<(), ModelServerError> {
        if let Some(activate) = self.activate.take() {
            activate.send(()).map_err(|_| ModelServerError::Task)?;
        }
        if let Some(running) = self.running.take() {
            running.await.map_err(|_| ModelServerError::Task)?;
        }
        if self.is_running() {
            Ok(())
        } else {
            Err(ModelServerError::Task)
        }
    }

    #[doc(hidden)]
    pub fn abort(&mut self) {
        self.status
            .store(ModelServerStatus::Failed.encode(), Ordering::Release);
        self.shutdown.take();
        self.activate.take();
        self.running.take();
        if let Some(task) = &self.task {
            task.abort();
        }
    }

    pub async fn shutdown(mut self) -> Result<(), ModelServerError> {
        self.shutdown_requested.store(true, Ordering::Release);
        self.activate.take();
        self.running.take();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            match task.await {
                Ok(Ok(())) if self.status() == ModelServerStatus::Stopped => {}
                Ok(Ok(())) | Ok(Err(_)) | Err(_) => {
                    self.status
                        .store(ModelServerStatus::Failed.encode(), Ordering::Release);
                    return Err(ModelServerError::Task);
                }
            }
        }
        Ok(())
    }
}

impl Drop for ModelServerHandle {
    fn drop(&mut self) {
        self.shutdown_requested.store(true, Ordering::Release);
        self.status
            .store(ModelServerStatus::Stopped.encode(), Ordering::Release);
        self.activate.take();
        self.running.take();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn route_responses(
    State(state): State<RouteState>,
    request: Request<Body>,
) -> Response<Body> {
    if state.admission.rejects_new_requests() {
        return local_response(StatusCode::SERVICE_UNAVAILABLE);
    }
    let expected = match state.store.routing_credential().await {
        Ok(Some(credential)) => credential,
        Ok(None) | Err(_) => return local_response(StatusCode::UNAUTHORIZED),
    };
    if !routing_credential_matches(request.headers(), &expected) {
        return local_response(StatusCode::UNAUTHORIZED);
    }

    let plan = match pin_route_plan(&state.store, state.target).await {
        Some(plan) => plan,
        None => return local_response(StatusCode::SERVICE_UNAVAILABLE),
    };
    let Some(mut recording) =
        state
            .request_recorder
            .begin(state.target, &plan, expected.expose_secret())
    else {
        return fixed_local_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "request-recording-unavailable",
        );
    };
    let Some(active_request) = ActiveRequestGuard::try_begin(Arc::clone(&state.admission)) else {
        recording.complete_terminal(
            Some(StatusCode::SERVICE_UNAVAILABLE),
            crate::control::protocol::RequestRecordOutcome::RouteUnavailable,
        );
        return local_response(StatusCode::SERVICE_UNAVAILABLE);
    };
    let request_headers = request.headers().clone();
    let mut incoming = request.into_body().into_data_stream();
    let mut request_body = Vec::new();
    while let Some(chunk) = incoming.next().await {
        let Ok(chunk) = chunk else {
            recording.complete_terminal(
                Some(StatusCode::BAD_REQUEST),
                crate::control::protocol::RequestRecordOutcome::RouteUnavailable,
            );
            return local_response(StatusCode::BAD_REQUEST);
        };
        if request_body.len().saturating_add(chunk.len()) > MAX_REPLAYABLE_BODY_BYTES {
            recording.complete_terminal(
                Some(StatusCode::PAYLOAD_TOO_LARGE),
                crate::control::protocol::RequestRecordOutcome::RouteUnavailable,
            );
            return local_response(StatusCode::PAYLOAD_TOO_LARGE);
        }
        request_body.extend_from_slice(&chunk);
    }
    let route = route_pinned_plan(
        plan,
        state.target,
        &state.route_health,
        &state.upstream,
        |member| {
            let prepared = (|| {
                if member.protocol != crate::control::protocol::ProviderProtocol::OpenaiResponses {
                    return None;
                }
                Some(PreparedRouteAttempt {
                    request: UpstreamRequest {
                        method: Method::POST,
                        url: responses_url(&member.base_url).ok()?,
                        headers: forward_request_headers(
                            &request_headers,
                            member.provider_credential.as_ref()?,
                        )
                        .ok()?,
                        body: ReqwestBody::from(request_body.clone()),
                    },
                    response_kind: RouteResponseKind::Native,
                })
            })()
            .ok_or(RouteAttemptFailure::Configuration);
            std::future::ready(prepared)
        },
    )
    .await;
    let outcome_without_response = route.request_record_outcome();
    let serving_provider = route
        .routed
        .as_ref()
        .filter(|routed| routed.response.status.is_success())
        .map(|routed| routed.provider_id);
    if !route.observations.is_empty() {
        let observations = route
            .observations
            .into_iter()
            .map(|observation| crate::state::RouteObservation {
                provider_id: observation.provider_id,
                state: observation.state.to_owned(),
                consecutive_successes: observation.consecutive_successes,
                consecutive_failures: observation.consecutive_failures,
                total_attempts: observation.total_attempts,
                failed_attempts: observation.failed_attempts,
                outcome: observation.outcome.as_str().to_owned(),
            })
            .collect();
        let _ = state
            .store
            .record_route_observations_for(
                state.target,
                route.plan_id,
                route.plan_epoch,
                observations,
                serving_provider,
            )
            .await;
    }
    let Some(routed) = route.routed else {
        if let Some(attempt) = route.last_attempt.as_ref() {
            recording.bind_attempt(attempt);
        }
        recording.complete_terminal(None, outcome_without_response);
        return local_response(StatusCode::BAD_GATEWAY);
    };
    recording.bind_routed(&routed);
    recording.configure_response(
        routed.response.status,
        routed.semantic_failure,
        ResponseUsageFormat::codex(
            routed
                .response
                .headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
        ),
        routed
            .response
            .headers
            .get(axum::http::header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.eq_ignore_ascii_case("gzip") || value.eq_ignore_ascii_case("x-gzip")
            }),
    );
    let upstream = routed.response;

    let mut response = Response::builder()
        .status(upstream.status)
        .body(recorded_body(
            upstream.body,
            active_request,
            recording,
            true,
        ))
        .expect("valid upstream status");
    *response.headers_mut() = forward_response_headers(&upstream.headers);
    response
}

pub(crate) struct ActiveRequestGuard {
    admission: Arc<ModelAdmission>,
}

impl ActiveRequestGuard {
    pub(crate) fn try_begin(admission: Arc<ModelAdmission>) -> Option<Self> {
        let mut observed = admission.state.load(Ordering::Acquire);
        loop {
            if observed & RESERVED_BIT != 0 {
                return None;
            }
            let next = observed.checked_add(ACTIVE_REQUEST_INCREMENT)?;
            match admission.state.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self { admission }),
                Err(actual) => observed = actual,
            }
        }
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        let previous = self
            .admission
            .state
            .fetch_sub(ACTIVE_REQUEST_INCREMENT, Ordering::AcqRel);
        if previous == RESERVED_BIT + ACTIVE_REQUEST_INCREMENT {
            self.admission.drained.notify_one();
        }
    }
}

fn local_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from("request rejected"))
        .expect("valid local response")
}

fn fixed_local_response(status: StatusCode, problem: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(problem))
        .expect("valid fixed local response")
}
