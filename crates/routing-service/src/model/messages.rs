use std::{io::Read, sync::Arc};

use axum::{
    body::Body,
    extract::State,
    http::{Method, Request, Response, StatusCode, header},
};
use flate2::read::MultiGzDecoder;
use futures_util::StreamExt;
use reqwest::Body as ReqwestBody;
use serde_json::Value;
use url::Url;

use super::{
    auth::bearer_routing_credential_matches,
    headers::{forward_claude_request_headers, forward_response_headers},
    router::{
        PreparedRouteAttempt, RouteAttemptFailure, RouteResponseKind, pin_route_plan,
        route_pinned_plan,
    },
    server::{ActiveRequestGuard, RouteState, body_with_active_guard},
    upstream::{UpstreamRequest, messages_url},
};
use crate::state::RouteObservation;
use crate::{
    control::protocol::{ProviderAuthentication, ProviderProtocol},
    subscription::resolver::SubscriptionAccountResolution,
    subscription_bridge::{BridgeRequestInput, SubscriptionBridgeAdapter},
};

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
    if state.admission.rejects_new_requests() {
        return local_response(StatusCode::SERVICE_UNAVAILABLE);
    }
    let expected = match state.store.routing_credential_for(state.target).await {
        Ok(Some(credential)) => credential,
        Ok(None) | Err(_) => return local_response(StatusCode::UNAUTHORIZED),
    };
    if !bearer_routing_credential_matches(request.headers(), &expected) {
        return local_response(StatusCode::UNAUTHORIZED);
    }
    let plan = match pin_route_plan(&state.store, state.target).await {
        Some(plan) => plan,
        None => return local_response(StatusCode::SERVICE_UNAVAILABLE),
    };
    let count_tokens = request.uri().path().ends_with("/count_tokens");
    let primary_is_bridge = plan
        .members
        .first()
        .is_some_and(|member| member.authentication == ProviderAuthentication::CodexSubscription);
    if count_tokens && primary_is_bridge {
        return fixed_failure_response(
            StatusCode::NOT_IMPLEMENTED,
            "subscription-bridge-count-tokens-unsupported",
        );
    }
    let Some(active_request) = ActiveRequestGuard::try_begin(Arc::clone(&state.admission)) else {
        return local_response(StatusCode::SERVICE_UNAVAILABLE);
    };
    let encoding = match content_encoding(request.headers()) {
        Ok(encoding) => encoding,
        Err(()) if primary_is_bridge => {
            return fixed_failure_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "subscription-bridge-invalid-request",
            );
        }
        Err(()) => return body_error(StatusCode::UNSUPPORTED_MEDIA_TYPE),
    };
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return if primary_is_bridge {
            fixed_failure_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "subscription-bridge-invalid-request",
            )
        } else {
            body_error(StatusCode::PAYLOAD_TOO_LARGE)
        };
    }
    let request_path = request.uri().path().to_owned();
    let query = request.uri().query().map(str::to_owned);
    let request_headers = request.headers().clone();
    let mut incoming = request.into_body().into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = incoming.next().await {
        let Ok(chunk) = chunk else {
            return if primary_is_bridge {
                fixed_failure_response(
                    StatusCode::BAD_REQUEST,
                    "subscription-bridge-invalid-request",
                )
            } else {
                body_error(StatusCode::BAD_REQUEST)
            };
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return if primary_is_bridge {
                fixed_failure_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "subscription-bridge-invalid-request",
                )
            } else {
                body_error(StatusCode::PAYLOAD_TOO_LARGE)
            };
        }
        bytes.extend_from_slice(&chunk);
    }
    let bytes = match decode_body(encoding, bytes) {
        Ok(bytes) => bytes,
        Err(BodyDecodeError::Invalid) if primary_is_bridge => {
            return fixed_failure_response(
                StatusCode::BAD_REQUEST,
                "subscription-bridge-invalid-request",
            );
        }
        Err(BodyDecodeError::TooLarge) if primary_is_bridge => {
            return fixed_failure_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "subscription-bridge-invalid-request",
            );
        }
        Err(BodyDecodeError::Invalid) => return body_error(StatusCode::BAD_REQUEST),
        Err(BodyDecodeError::TooLarge) => return body_error(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let object = match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(object)) => object,
        _ if primary_is_bridge => {
            return fixed_failure_response(
                StatusCode::BAD_REQUEST,
                "subscription-bridge-invalid-request",
            );
        }
        _ => return body_error(StatusCode::BAD_REQUEST),
    };
    let route = route_pinned_plan(
        plan,
        state.target,
        &state.route_health,
        &state.upstream,
        |member| {
            let protocol = member.protocol;
            let authentication = member.authentication;
            let base_url = member.base_url.clone();
            let model = member.model.clone();
            let credential = member.provider_credential.clone();
            let binding = member.subscription_binding.clone();
            let resolver = state.subscription_resolver.clone();
            let object = object.clone();
            let bytes = bytes.clone();
            let request_headers = request_headers.clone();
            let query = query.clone();
            let request_path = request_path.clone();
            async move {
                if protocol != ProviderProtocol::AnthropicMessages {
                    return Err(RouteAttemptFailure::Configuration);
                }
                if authentication == ProviderAuthentication::CodexSubscription {
                    let resolver =
                        resolver.ok_or(RouteAttemptFailure::SubscriptionAccountUnavailable)?;
                    let binding = binding
                        .as_ref()
                        .ok_or(RouteAttemptFailure::SubscriptionAccountUnavailable)?;
                    let access = resolver
                        .resolve_subscription_account(binding)
                        .await
                        .map_err(|error| match error {
                            SubscriptionAccountResolution::Unavailable => {
                                RouteAttemptFailure::SubscriptionAccountUnavailable
                            }
                            SubscriptionAccountResolution::NeedsReauthorization => {
                                RouteAttemptFailure::SubscriptionAccountNeedsReauthorization
                            }
                        })?;
                    let prepared = SubscriptionBridgeAdapter::prepare(BridgeRequestInput {
                        path: &request_path,
                        provider_model: &model,
                        account_id: access.account_id(),
                        access_token: access.access_token(),
                        inbound_headers: &request_headers,
                        body: &bytes,
                    })
                    .map_err(|_| RouteAttemptFailure::SubscriptionBridgeInvalidRequest)?;
                    return Ok(PreparedRouteAttempt {
                        request: UpstreamRequest {
                            method: Method::POST,
                            url: Url::parse(prepared.url())
                                .map_err(|_| RouteAttemptFailure::Configuration)?,
                            headers: prepared.headers().clone(),
                            body: ReqwestBody::from(
                                serde_json::to_vec(prepared.body())
                                    .map_err(|_| RouteAttemptFailure::Configuration)?,
                            ),
                        },
                        response_kind: RouteResponseKind::SubscriptionBridge,
                    });
                }
                let mut body = object;
                body.insert("model".to_owned(), Value::String(model));
                Ok(PreparedRouteAttempt {
                    request: UpstreamRequest {
                        method: Method::POST,
                        url: messages_url(&base_url, count_tokens, query.as_deref())
                            .map_err(|_| RouteAttemptFailure::Configuration)?,
                        headers: forward_claude_request_headers(
                            &request_headers,
                            authentication,
                            credential
                                .as_ref()
                                .ok_or(RouteAttemptFailure::Configuration)?,
                        )
                        .map_err(|_| RouteAttemptFailure::Configuration)?,
                        body: ReqwestBody::from(
                            serde_json::to_vec(&Value::Object(body))
                                .map_err(|_| RouteAttemptFailure::Configuration)?,
                        ),
                    },
                    response_kind: RouteResponseKind::Native,
                })
            }
        },
    )
    .await;
    let serving_provider = route
        .routed
        .as_ref()
        .filter(|routed| routed.response.status.is_success())
        .map(|routed| routed.provider_id);
    if !route.observations.is_empty() {
        let observations = route
            .observations
            .into_iter()
            .map(|observation| RouteObservation {
                provider_id: observation.provider_id,
                state: observation.state.to_owned(),
                consecutive_successes: observation.consecutive_successes,
                consecutive_failures: observation.consecutive_failures,
                total_attempts: observation.total_attempts,
                failed_attempts: observation.failed_attempts,
                outcome: observation.outcome.to_owned(),
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
        return fixed_failure_response(
            if route.failure == Some(RouteAttemptFailure::SubscriptionBridgeInvalidRequest) {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            },
            route
                .failure
                .map(RouteAttemptFailure::code)
                .unwrap_or("model-route-unavailable"),
        );
    };
    let mut upstream = routed.response;
    if routed.response_kind == RouteResponseKind::SubscriptionBridge {
        if upstream.status.is_success() {
            let converted = SubscriptionBridgeAdapter::convert_stream(upstream.body);
            upstream.body = Box::pin(converted.map(Ok));
            upstream.headers = axum::http::HeaderMap::new();
            upstream.headers.insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/event-stream"),
            );
        } else {
            let status = upstream.status;
            let mut error_body = Vec::new();
            while let Some(chunk) = upstream.body.next().await {
                let Ok(chunk) = chunk else {
                    return fixed_bridge_response(status, active_request);
                };
                if error_body.len().saturating_add(chunk.len())
                    > crate::subscription_bridge::MAX_BRIDGE_ERROR_BODY_BYTES
                {
                    return fixed_bridge_response(status, active_request);
                }
                error_body.extend_from_slice(&chunk);
            }
            let failure = match SubscriptionBridgeAdapter::map_non_success(status, &error_body) {
                Ok(failure) => failure,
                Err(_) => {
                    return fixed_bridge_response(status, active_request);
                }
            };
            upstream.status = failure.status();
            upstream.body = Box::pin(futures_util::stream::once(std::future::ready(Ok(failure
                .event()
                .clone()))));
            upstream.headers = axum::http::HeaderMap::new();
            upstream.headers.insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/event-stream"),
            );
        }
    }
    let mut response = Response::builder()
        .status(upstream.status)
        .body(body_with_active_guard(upstream.body, active_request))
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

fn fixed_failure_response(status: StatusCode, code: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(code))
        .expect("valid fixed failure response")
}

fn fixed_bridge_response(status: StatusCode, active_request: ActiveRequestGuard) -> Response<Body> {
    let failure = SubscriptionBridgeAdapter::invalid_response(status);
    let body = futures_util::stream::once(std::future::ready(Ok(failure.event().clone())));
    Response::builder()
        .status(failure.status())
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(body_with_active_guard(Box::pin(body), active_request))
        .expect("valid fixed bridge failure response")
}
