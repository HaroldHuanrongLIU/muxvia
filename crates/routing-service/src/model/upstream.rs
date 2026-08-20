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
    subscription_bridge_test_origin: Option<Url>,
}

impl ReqwestUpstream {
    pub fn new() -> Result<Self, UpstreamError> {
        Self::new_with_subscription_bridge_test_origin(None)
    }

    pub(crate) fn new_with_subscription_bridge_test_origin(
        origin: Option<&str>,
    ) -> Result<Self, UpstreamError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| UpstreamError)?;
        let subscription_bridge_test_origin = origin.map(parse_test_origin).transpose()?;
        Ok(Self {
            client,
            subscription_bridge_test_origin,
        })
    }
}

#[async_trait::async_trait]
impl UpstreamTransport for ReqwestUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let url = if request.url.scheme() == "https"
            && request.url.host_str() == Some("chatgpt.com")
            && request.url.path() == "/backend-api/codex/responses"
        {
            match &self.subscription_bridge_test_origin {
                Some(origin) => {
                    let mut rewritten = origin.clone();
                    rewritten.set_path(request.url.path());
                    rewritten.set_query(request.url.query());
                    rewritten
                }
                None => request.url,
            }
        } else {
            request.url
        };
        let response = self
            .client
            .request(request.method, url)
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

fn parse_test_origin(origin: &str) -> Result<Url, UpstreamError> {
    let url = Url::parse(origin).map_err(|_| UpstreamError)?;
    let loopback = url
        .host_str()
        .and_then(|host| host.parse::<std::net::IpAddr>().ok())
        .is_some_and(|address| address.is_loopback());
    if url.scheme() != "http"
        || !loopback
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(UpstreamError);
    }
    Ok(url)
}

pub fn responses_url(base_url: &str) -> Result<Url, UpstreamError> {
    let normalized = base_url.trim_end_matches('/');
    Url::parse(&format!("{normalized}/responses")).map_err(|_| UpstreamError)
}

pub fn messages_url(
    base_url: &str,
    count_tokens: bool,
    query: Option<&str>,
) -> Result<Url, UpstreamError> {
    let normalized = base_url.trim_end_matches('/');
    let suffix = if count_tokens {
        "/messages/count_tokens"
    } else {
        "/messages"
    };
    let mut url = Url::parse(&format!("{normalized}{suffix}")).map_err(|_| UpstreamError)?;
    url.set_query(query);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::ReqwestUpstream;

    #[test]
    fn subscription_bridge_test_origin_accepts_only_a_plain_loopback_root() {
        assert!(
            ReqwestUpstream::new_with_subscription_bridge_test_origin(Some(
                "http://127.0.0.1:41234/"
            ))
            .is_ok(),
            "valid loopback test origin was rejected"
        );
        for origin in [
            "https://127.0.0.1:41234/",
            "http://example.test:41234/",
            "http://127.0.0.1:41234/path",
            "http://user@127.0.0.1:41234/",
            "http://127.0.0.1:41234/?query=1",
        ] {
            assert!(
                ReqwestUpstream::new_with_subscription_bridge_test_origin(Some(origin)).is_err(),
                "unsafe Subscription Bridge test origin was accepted"
            );
        }
    }
}
