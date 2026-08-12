use std::{pin::Pin, time::Duration};

use axum::{
    body::Bytes,
    http::{HeaderMap, Method, StatusCode},
};
use futures_util::{Stream, StreamExt};
use reqwest::{Body, Url};

pub struct UpstreamRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Body,
}

pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Pin<Box<dyn Stream<Item = Result<Bytes, UpstreamError>> + Send>>,
}

#[derive(Debug, thiserror::Error)]
#[error("upstream request failed")]
pub struct UpstreamError;

#[async_trait::async_trait]
pub trait UpstreamTransport: Send + Sync {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError>;
}

pub struct ReqwestUpstream {
    client: reqwest::Client,
}

impl ReqwestUpstream {
    pub fn new() -> Result<Self, UpstreamError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| UpstreamError)?;
        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl UpstreamTransport for ReqwestUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let response = self
            .client
            .request(request.method, request.url)
            .headers(request.headers)
            .body(request.body)
            .send()
            .await
            .map_err(|_| UpstreamError)?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .bytes_stream()
            .map(|result| result.map_err(|_| UpstreamError));
        Ok(UpstreamResponse {
            status,
            headers,
            body: Box::pin(body),
        })
    }
}

pub fn responses_url(base_url: &str) -> Result<Url, UpstreamError> {
    let normalized = base_url.trim_end_matches('/');
    Url::parse(&format!("{normalized}/responses")).map_err(|_| UpstreamError)
}
