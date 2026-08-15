use std::collections::HashSet;

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use secrecy::{ExposeSecret, SecretString};

use super::auth::ROUTING_CREDENTIAL_HEADER;
use crate::control::protocol::ProviderAuthentication;

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Debug, thiserror::Error)]
#[error("invalid upstream authorization metadata")]
pub struct HeaderPolicyError;

pub fn forward_request_headers(
    incoming: &HeaderMap,
    provider_credential: &SecretString,
) -> Result<HeaderMap, HeaderPolicyError> {
    let mut blocked = blocked_names(incoming);
    blocked.extend([
        header::HOST,
        header::CONTENT_LENGTH,
        header::AUTHORIZATION,
        HeaderName::from_static(ROUTING_CREDENTIAL_HEADER),
    ]);
    let mut forwarded = copy_except(incoming, &blocked);
    let authorization =
        HeaderValue::from_str(&format!("Bearer {}", provider_credential.expose_secret()))
            .map_err(|_| HeaderPolicyError)?;
    forwarded.insert(header::AUTHORIZATION, authorization);
    Ok(forwarded)
}

pub fn forward_claude_request_headers(
    incoming: &HeaderMap,
    authentication: ProviderAuthentication,
    provider_credential: &SecretString,
) -> Result<HeaderMap, HeaderPolicyError> {
    let mut blocked = blocked_names(incoming);
    blocked.extend([
        header::HOST,
        header::CONTENT_LENGTH,
        header::CONTENT_ENCODING,
        header::AUTHORIZATION,
        HeaderName::from_static("x-api-key"),
        HeaderName::from_static(ROUTING_CREDENTIAL_HEADER),
    ]);
    let mut forwarded = copy_except(incoming, &blocked);
    let value = HeaderValue::from_str(provider_credential.expose_secret())
        .map_err(|_| HeaderPolicyError)?;
    match authentication {
        ProviderAuthentication::AnthropicApiKey => {
            forwarded.insert(HeaderName::from_static("x-api-key"), value);
        }
        ProviderAuthentication::AnthropicBearer => {
            let authorization =
                HeaderValue::from_str(&format!("Bearer {}", provider_credential.expose_secret()))
                    .map_err(|_| HeaderPolicyError)?;
            forwarded.insert(header::AUTHORIZATION, authorization);
        }
        ProviderAuthentication::OpenaiBearer => return Err(HeaderPolicyError),
    }
    Ok(forwarded)
}

pub fn forward_response_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut blocked = blocked_names(incoming);
    blocked.insert(header::CONTENT_LENGTH);
    copy_except(incoming, &blocked)
}

fn blocked_names(headers: &HeaderMap) -> HashSet<HeaderName> {
    let mut blocked = HOP_BY_HOP
        .iter()
        .map(|name| HeaderName::from_static(name))
        .collect::<HashSet<_>>();
    for value in headers.get_all(header::CONNECTION) {
        for token in value.as_bytes().split(|byte| *byte == b',') {
            let token = trim_ascii_whitespace(token);
            if let Ok(name) = HeaderName::from_bytes(token) {
                blocked.insert(name);
            }
        }
    }
    blocked
}

fn copy_except(headers: &HeaderMap, blocked: &HashSet<HeaderName>) -> HeaderMap {
    let mut copied = HeaderMap::new();
    for (name, value) in headers {
        if !blocked.contains(name) && !name.as_str().starts_with("proxy-") {
            copied.append(name, value.clone());
        }
    }
    copied
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}
