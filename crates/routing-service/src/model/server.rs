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
use futures_util::{StreamExt, stream};
use reqwest::Body as ReqwestBody;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

use crate::control::protocol::Target;
use crate::state::StateStore;

use super::messages::route_messages;
use super::{
    auth::routing_credential_matches,
    headers::{forward_request_headers, forward_response_headers},
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

pub struct ModelServerHandle {
    endpoint: SocketAddr,
    activate: Option<oneshot::Sender<()>>,
    running: Option<oneshot::Receiver<()>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), io::Error>>>,
    status: Arc<AtomicU8>,
    shutdown_requested: Arc<AtomicBool>,
    admission: Arc<AtomicUsize>,
    #[cfg(test)]
    reservation_attempt_hook: Option<Arc<dyn Fn() + Send + Sync>>,
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
    pub(crate) admission: Arc<AtomicUsize>,
}

impl Clone for RouteState {
    fn clone(&self) -> Self {
        Self {
            target: self.target,
            store: Arc::clone(&self.store),
            upstream: Arc::clone(&self.upstream),
            admission: Arc::clone(&self.admission),
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
        let mut handle = Self::bind_reserved_staged_for(reserved, target, store, upstream).await?;
        handle.activate().await?;
        Ok(handle)
    }

    pub async fn bind_reserved_staged_for(
        reserved: ReservedListener,
        target: Target,
        store: Arc<StateStore>,
        upstream: Arc<dyn UpstreamTransport>,
    ) -> Result<ModelServerHandle, ModelServerError> {
        let endpoint = reserved.endpoint;
        let state = RouteState {
            target,
            store,
            upstream,
            admission: Arc::new(AtomicUsize::new(0)),
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
            let result = axum::serve(reserved.listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
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
        self.admission.load(Ordering::Acquire) / ACTIVE_REQUEST_INCREMENT
    }

    pub(crate) fn try_reserve_idle(&self) -> bool {
        let reserved = self
            .admission
            .compare_exchange(0, RESERVED_BIT, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        #[cfg(test)]
        if let Some(hook) = &self.reservation_attempt_hook {
            hook();
        }
        reserved
    }

    pub(crate) fn release_idle_reservation(&self) {
        let released =
            self.admission
                .compare_exchange(RESERVED_BIT, 0, Ordering::AcqRel, Ordering::Acquire);
        debug_assert!(released.is_ok());
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
    let expected = match state.store.routing_credential().await {
        Ok(Some(credential)) => credential,
        Ok(None) | Err(_) => return local_response(StatusCode::UNAUTHORIZED),
    };
    if !routing_credential_matches(request.headers(), &expected) {
        return local_response(StatusCode::UNAUTHORIZED);
    }

    let snapshot = match state.store.activated_snapshot().await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) | Err(_) => return local_response(StatusCode::SERVICE_UNAVAILABLE),
    };
    let Some(active_request) = ActiveRequestGuard::try_begin(Arc::clone(&state.admission)) else {
        return local_response(StatusCode::SERVICE_UNAVAILABLE);
    };
    let url = match responses_url(snapshot.base_url()) {
        Ok(url) => url,
        Err(_) => return local_response(StatusCode::BAD_GATEWAY),
    };
    let headers = match forward_request_headers(request.headers(), snapshot.provider_credential()) {
        Ok(headers) => headers,
        Err(_) => return local_response(StatusCode::BAD_GATEWAY),
    };
    let body = ReqwestBody::wrap_stream(request.into_body().into_data_stream());
    let upstream = match state
        .upstream
        .send(UpstreamRequest {
            method: Method::POST,
            url,
            headers,
            body,
        })
        .await
    {
        Ok(response) => response,
        Err(_) => return local_response(StatusCode::BAD_GATEWAY),
    };

    if upstream.status.is_success() {
        let _ = state.store.record_serving(snapshot.id()).await;
    }

    let mut response = Response::builder()
        .status(upstream.status)
        .body(body_with_active_guard(upstream.body, active_request))
        .expect("valid upstream status");
    *response.headers_mut() = forward_response_headers(&upstream.headers);
    response
}

pub(crate) struct ActiveRequestGuard {
    admission: Arc<AtomicUsize>,
}

impl ActiveRequestGuard {
    pub(crate) fn try_begin(admission: Arc<AtomicUsize>) -> Option<Self> {
        let mut observed = admission.load(Ordering::Acquire);
        loop {
            if observed & RESERVED_BIT != 0 {
                return None;
            }
            let next = observed.checked_add(ACTIVE_REQUEST_INCREMENT)?;
            match admission.compare_exchange_weak(
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
        self.admission
            .fetch_sub(ACTIVE_REQUEST_INCREMENT, Ordering::AcqRel);
    }
}

pub(crate) fn body_with_active_guard(
    body: std::pin::Pin<
        Box<
            dyn futures_util::Stream<Item = Result<axum::body::Bytes, super::UpstreamError>> + Send,
        >,
    >,
    guard: ActiveRequestGuard,
) -> Body {
    let guarded = stream::unfold((body, Some(guard)), |(mut body, guard)| async move {
        body.next().await.map(|item| (item, (body, guard)))
    });
    Body::from_stream(guarded)
}

fn local_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from("request rejected"))
        .expect("valid local response")
}
