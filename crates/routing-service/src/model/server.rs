use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
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
    task: Option<JoinHandle<()>>,
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
        let task = tokio::spawn(async move {
            let _ = axum::serve(reserved.listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        Ok(ModelServerHandle {
            endpoint,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }
}

impl ModelServerHandle {
    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    pub async fn shutdown(mut self) -> Result<(), ModelServerError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.map_err(|_| ModelServerError::Task)?;
        }
        Ok(())
    }
}

impl Drop for ModelServerHandle {
    fn drop(&mut self) {
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
