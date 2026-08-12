use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Method, Request, Response, StatusCode},
    routing::post,
};
use reqwest::Body as ReqwestBody;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

use crate::state::StateStore;

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

pub struct ModelServerHandle {
    endpoint: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), io::Error>>>,
    status: Arc<AtomicU8>,
    shutdown_requested: Arc<AtomicBool>,
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

struct RouteState {
    store: Arc<StateStore>,
    upstream: Arc<dyn UpstreamTransport>,
}

impl Clone for RouteState {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            upstream: Arc::clone(&self.upstream),
        }
    }
}

impl ModelServer {
    pub async fn bind_reserved(
        reserved: ReservedListener,
        store: Arc<StateStore>,
        upstream: Arc<dyn UpstreamTransport>,
    ) -> Result<ModelServerHandle, ModelServerError> {
        let endpoint = reserved.endpoint;
        let router = Router::new()
            .route("/v1/responses", post(route_responses))
            .with_state(RouteState { store, upstream });
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let status = Arc::new(AtomicU8::new(ModelServerStatus::Starting.encode()));
        let task_status = Arc::clone(&status);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let task_shutdown_requested = Arc::clone(&shutdown_requested);
        let task = tokio::spawn(async move {
            task_status.store(ModelServerStatus::Running.encode(), Ordering::Release);
            let _ = ready_tx.send(());
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
        if ready_rx.await.is_err() {
            status.store(ModelServerStatus::Failed.encode(), Ordering::Release);
            return Err(ModelServerError::Task);
        }
        let handle = ModelServerHandle {
            endpoint,
            shutdown: Some(shutdown),
            task: Some(task),
            status,
            shutdown_requested,
        };
        if !handle.is_running() {
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

    #[doc(hidden)]
    pub fn abort(&mut self) {
        self.status
            .store(ModelServerStatus::Failed.encode(), Ordering::Release);
        self.shutdown.take();
        if let Some(task) = &self.task {
            task.abort();
        }
    }

    pub async fn shutdown(mut self) -> Result<(), ModelServerError> {
        self.shutdown_requested.store(true, Ordering::Release);
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

    if upstream.status.is_success() && state.store.record_serving(snapshot.id()).await.is_err() {
        return local_response(StatusCode::BAD_GATEWAY);
    }

    let mut response = Response::builder()
        .status(upstream.status)
        .body(Body::from_stream(upstream.body))
        .expect("valid upstream status");
    *response.headers_mut() = forward_response_headers(&upstream.headers);
    response
}

fn local_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from("request rejected"))
        .expect("valid local response")
}
