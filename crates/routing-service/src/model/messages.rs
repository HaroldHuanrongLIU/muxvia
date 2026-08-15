use std::io::Read;

use axum::{
    body::Body,
    extract::State,
    http::{Method, Request, Response, StatusCode, header},
};
use flate2::read::MultiGzDecoder;
use futures_util::StreamExt;
use reqwest::Body as ReqwestBody;
use serde_json::Value;

use super::{
    auth::bearer_routing_credential_matches,
    headers::{forward_claude_request_headers, forward_response_headers},
    server::RouteState,
    upstream::{UpstreamRequest, messages_url},
};
use crate::control::protocol::ProviderProtocol;

const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
enum ContentEncoding {
    Identity,
    Gzip,
}

pub(crate) async fn route_messages(
    State(state): State<RouteState>,
    request: Request<Body>,
) -> Response<Body> {
    let expected = match state.store.routing_credential_for(state.target).await {
        Ok(Some(credential)) => credential,
        Ok(None) | Err(_) => return local_response(StatusCode::UNAUTHORIZED),
    };
    if !bearer_routing_credential_matches(request.headers(), &expected) {
        return local_response(StatusCode::UNAUTHORIZED);
    }
    let snapshot = match state.store.activated_snapshot_for(state.target).await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) | Err(_) => return local_response(StatusCode::SERVICE_UNAVAILABLE),
    };
    if snapshot.protocol() != ProviderProtocol::AnthropicMessages {
        return local_response(StatusCode::SERVICE_UNAVAILABLE);
    }
    let encoding = match content_encoding(request.headers()) {
        Ok(encoding) => encoding,
        Err(()) => return body_error(StatusCode::UNSUPPORTED_MEDIA_TYPE),
    };
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return body_error(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let count_tokens = request.uri().path().ends_with("/count_tokens");
    let url = match messages_url(snapshot.base_url(), count_tokens, request.uri().query()) {
        Ok(url) => url,
        Err(_) => return local_response(StatusCode::BAD_GATEWAY),
    };
    let headers = match forward_claude_request_headers(
        request.headers(),
        snapshot.authentication(),
        snapshot.provider_credential(),
    ) {
        Ok(headers) => headers,
        Err(_) => return local_response(StatusCode::BAD_GATEWAY),
    };
    let mut incoming = request.into_body().into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = incoming.next().await {
        let Ok(chunk) = chunk else {
            return body_error(StatusCode::BAD_REQUEST);
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return body_error(StatusCode::PAYLOAD_TOO_LARGE);
        }
        bytes.extend_from_slice(&chunk);
    }
    let bytes = match decode_body(encoding, bytes) {
        Ok(bytes) => bytes,
        Err(BodyDecodeError::Invalid) => return body_error(StatusCode::BAD_REQUEST),
        Err(BodyDecodeError::TooLarge) => return body_error(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let mut object = match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(object)) => object,
        _ => return body_error(StatusCode::BAD_REQUEST),
    };
    object.insert(
        "model".to_owned(),
        Value::String(snapshot.model().to_owned()),
    );
    let body = match serde_json::to_vec(&Value::Object(object)) {
        Ok(body) => body,
        Err(_) => return body_error(StatusCode::BAD_REQUEST),
    };
    let upstream = match state
        .upstream
        .send(UpstreamRequest {
            method: Method::POST,
            url,
            headers,
            body: ReqwestBody::from(body),
        })
        .await
    {
        Ok(response) => response,
        Err(_) => return local_response(StatusCode::BAD_GATEWAY),
    };
    if upstream.status.is_success() {
        let _ = state
            .store
            .record_serving_for(state.target, snapshot.id())
            .await;
    }
    let mut response = Response::builder()
        .status(upstream.status)
        .body(Body::from_stream(upstream.body))
        .expect("valid upstream status");
    *response.headers_mut() = forward_response_headers(&upstream.headers);
    response
}

fn content_encoding(headers: &axum::http::HeaderMap) -> Result<ContentEncoding, ()> {
    let mut values = headers.get_all(header::CONTENT_ENCODING).iter();
    let Some(value) = values.next() else {
        return Ok(ContentEncoding::Identity);
    };
    if values.next().is_some() {
        return Err(());
    }
    let value = value.to_str().map_err(|_| ())?.trim();
    if value.eq_ignore_ascii_case("identity") {
        Ok(ContentEncoding::Identity)
    } else if value.eq_ignore_ascii_case("gzip") || value.eq_ignore_ascii_case("x-gzip") {
        Ok(ContentEncoding::Gzip)
    } else {
        Err(())
    }
}

enum BodyDecodeError {
    Invalid,
    TooLarge,
}

fn decode_body(encoding: ContentEncoding, bytes: Vec<u8>) -> Result<Vec<u8>, BodyDecodeError> {
    match encoding {
        ContentEncoding::Identity => Ok(bytes),
        ContentEncoding::Gzip => {
            let mut decoder =
                MultiGzDecoder::new(bytes.as_slice()).take((MAX_BODY_BYTES + 1) as u64);
            let mut decoded = Vec::new();
            decoder
                .read_to_end(&mut decoded)
                .map_err(|_| BodyDecodeError::Invalid)?;
            if decoded.len() > MAX_BODY_BYTES {
                Err(BodyDecodeError::TooLarge)
            } else {
                Ok(decoded)
            }
        }
    }
}

fn body_error(status: StatusCode) -> Response<Body> {
    let body = match status {
        StatusCode::BAD_REQUEST => "invalid request body",
        StatusCode::PAYLOAD_TOO_LARGE => "request body too large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported content encoding",
        _ => "request rejected",
    };
    Response::builder()
        .status(status)
        .body(Body::from(body))
        .expect("valid local response")
}

fn local_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from("request rejected"))
        .expect("valid local response")
}
