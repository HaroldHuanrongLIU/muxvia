use std::{io::Read, time::Duration};

use axum::{body::Bytes, http::header};
use flate2::read::MultiGzDecoder;
use futures_util::{StreamExt, stream};
use serde_json::Value;

use crate::control::protocol::Target;

use super::{UpstreamError, UpstreamResponse};

const PRIMING_BYTE_LIMIT: usize = 1024 * 1024;
const PRIMING_TIME_LIMIT: Duration = Duration::from_millis(250);

pub(crate) enum PrimedResponse {
    Committed(UpstreamResponse),
    Retry,
}

pub(crate) async fn prime_success_response(
    response: UpstreamResponse,
    target: Target,
) -> PrimedResponse {
    let Some(content_type) = normalized_header(&response, header::CONTENT_TYPE) else {
        return PrimedResponse::Retry;
    };
    if content_type == "text/event-stream" {
        prime_sse(response, target).await
    } else if content_type == "application/json" || content_type.ends_with("+json") {
        prime_json(response, target).await
    } else {
        PrimedResponse::Retry
    }
}

fn normalized_header(response: &UpstreamResponse, name: header::HeaderName) -> Option<String> {
    let raw = response.headers.get(name)?.to_str().ok()?;
    Some(raw.split(';').next()?.trim().to_ascii_lowercase())
}

async fn prime_sse(mut response: UpstreamResponse, target: Target) -> PrimedResponse {
    if let Some(encoding) = normalized_header(&response, header::CONTENT_ENCODING) {
        if encoding == "gzip" || encoding == "x-gzip" {
            return prime_compressed_sse(response, target).await;
        }
        if encoding != "identity" {
            return PrimedResponse::Committed(response);
        }
    }

    let started = tokio::time::Instant::now();
    let mut buffered = Vec::new();
    let mut semantic = Vec::new();
    let mut parsed = 0;
    loop {
        let Some(remaining) = PRIMING_TIME_LIMIT.checked_sub(started.elapsed()) else {
            return committed_with_prefix(response, buffered);
        };
        let next = match tokio::time::timeout(remaining, response.body.next()).await {
            Ok(next) => next,
            Err(_) => return committed_with_prefix(response, buffered),
        };
        let Some(next) = next else {
            return PrimedResponse::Retry;
        };
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(_) => return PrimedResponse::Retry,
        };
        if semantic.len().saturating_add(chunk.len()) > PRIMING_BYTE_LIMIT {
            buffered.push(chunk);
            return committed_with_prefix(response, buffered);
        }
        semantic.extend_from_slice(&chunk);
        buffered.push(chunk);
        while let Some((event_end, separator_len)) = next_event(&semantic, parsed) {
            let event = &semantic[parsed..event_end];
            parsed = event_end + separator_len;
            match classify_sse_event(event, target) {
                EventClass::Lifecycle => {}
                EventClass::Productive | EventClass::NonFailureTerminal => {
                    return committed_with_prefix(response, buffered);
                }
                EventClass::Failure | EventClass::Malformed => return PrimedResponse::Retry,
            }
        }
    }
}

async fn prime_compressed_sse(mut response: UpstreamResponse, target: Target) -> PrimedResponse {
    let bytes = match collect_bounded(&mut response).await {
        BoundedCollection::Complete(bytes) => bytes,
        BoundedCollection::Commit(prefix) => return committed_with_prefix(response, prefix),
        BoundedCollection::Failure => return PrimedResponse::Retry,
    };
    let mut decoder = MultiGzDecoder::new(bytes.as_slice()).take((PRIMING_BYTE_LIMIT + 1) as u64);
    let mut decoded = Vec::new();
    if decoder.read_to_end(&mut decoded).is_err() || decoded.len() > PRIMING_BYTE_LIMIT {
        return PrimedResponse::Retry;
    }
    match classify_complete_sse(&decoded, target) {
        EventClass::Failure | EventClass::Malformed | EventClass::Lifecycle => {
            PrimedResponse::Retry
        }
        EventClass::Productive | EventClass::NonFailureTerminal => {
            committed_with_prefix(response, vec![Bytes::from(bytes)])
        }
    }
}

async fn prime_json(mut response: UpstreamResponse, target: Target) -> PrimedResponse {
    if let Some(encoding) = normalized_header(&response, header::CONTENT_ENCODING)
        && (encoding == "gzip" || encoding == "x-gzip")
    {
        return prime_compressed_json(response, target).await;
    }
    let bytes = match collect_bounded(&mut response).await {
        BoundedCollection::Complete(bytes) => bytes,
        BoundedCollection::Commit(prefix) => return committed_with_prefix(response, prefix),
        BoundedCollection::Failure => return PrimedResponse::Retry,
    };
    let valid = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .is_some_and(|value| valid_nonstreaming_value(&value, target));
    if valid {
        committed_with_prefix(response, vec![Bytes::from(bytes)])
    } else {
        PrimedResponse::Retry
    }
}

async fn prime_compressed_json(mut response: UpstreamResponse, target: Target) -> PrimedResponse {
    let bytes = match collect_bounded(&mut response).await {
        BoundedCollection::Complete(bytes) => bytes,
        BoundedCollection::Commit(prefix) => return committed_with_prefix(response, prefix),
        BoundedCollection::Failure => return PrimedResponse::Retry,
    };
    let mut decoder = MultiGzDecoder::new(bytes.as_slice()).take((PRIMING_BYTE_LIMIT + 1) as u64);
    let mut decoded = Vec::new();
    if decoder.read_to_end(&mut decoded).is_err() || decoded.len() > PRIMING_BYTE_LIMIT {
        return PrimedResponse::Retry;
    }
    let valid = serde_json::from_slice::<Value>(&decoded)
        .ok()
        .is_some_and(|value| valid_nonstreaming_value(&value, target));
    if valid {
        committed_with_prefix(response, vec![Bytes::from(bytes)])
    } else {
        PrimedResponse::Retry
    }
}

enum BoundedCollection {
    Complete(Vec<u8>),
    Commit(Vec<Bytes>),
    Failure,
}

async fn collect_bounded(response: &mut UpstreamResponse) -> BoundedCollection {
    let started = tokio::time::Instant::now();
    let mut bytes = Vec::new();
    let mut chunks = Vec::new();
    loop {
        let Some(remaining) = PRIMING_TIME_LIMIT.checked_sub(started.elapsed()) else {
            return BoundedCollection::Commit(chunks);
        };
        let next = match tokio::time::timeout(remaining, response.body.next()).await {
            Ok(next) => next,
            Err(_) => return BoundedCollection::Commit(chunks),
        };
        let Some(next) = next else {
            return BoundedCollection::Complete(bytes);
        };
        let chunk = match next {
            Ok(chunk) => chunk,
            Err(_) => return BoundedCollection::Failure,
        };
        if bytes.len().saturating_add(chunk.len()) > PRIMING_BYTE_LIMIT {
            chunks.push(chunk);
            return BoundedCollection::Commit(chunks);
        }
        bytes.extend_from_slice(&chunk);
        chunks.push(chunk);
    }
}

fn valid_nonstreaming_value(value: &Value, target: Target) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.contains_key("error") || object.get("type").and_then(Value::as_str) == Some("error") {
        return false;
    }
    match target {
        Target::Codex => !matches!(
            object.get("status").and_then(Value::as_str),
            Some("failed" | "incomplete")
        ),
        Target::Claude => true,
    }
}

fn classify_complete_sse(bytes: &[u8], target: Target) -> EventClass {
    let mut parsed = 0;
    let mut last = EventClass::Lifecycle;
    while let Some((event_end, separator_len)) = next_event(bytes, parsed) {
        last = classify_sse_event(&bytes[parsed..event_end], target);
        if !matches!(last, EventClass::Lifecycle) {
            return last;
        }
        parsed = event_end + separator_len;
    }
    if parsed != bytes.len() {
        EventClass::Malformed
    } else {
        last
    }
}

fn next_event(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let tail = bytes.get(from..)?;
    for index in 0..tail.len() {
        if tail[index..].starts_with(b"\r\n\r\n") {
            return Some((from + index, 4));
        }
        if tail[index..].starts_with(b"\n\n") {
            return Some((from + index, 2));
        }
    }
    None
}

#[derive(Clone, Copy)]
enum EventClass {
    Lifecycle,
    Productive,
    NonFailureTerminal,
    Failure,
    Malformed,
}

fn classify_sse_event(bytes: &[u8], target: Target) -> EventClass {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return EventClass::Malformed;
    };
    let mut data = String::new();
    let mut saw_data = false;
    let mut wire_event_type = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("data:") {
            if saw_data {
                data.push('\n');
            }
            saw_data = true;
            data.push_str(value.trim_start());
        } else if let Some(value) = line.strip_prefix("event:") {
            let value = value.trim();
            if value.is_empty() || wire_event_type.replace(value).is_some() {
                return EventClass::Malformed;
            }
        }
    }
    if !saw_data {
        return if wire_event_type.is_none() {
            EventClass::Lifecycle
        } else {
            EventClass::Malformed
        };
    }
    if data == "[DONE]" {
        return EventClass::NonFailureTerminal;
    }
    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return EventClass::Malformed;
    };
    let json_event_type = value.get("type").and_then(Value::as_str);
    if matches!((wire_event_type, json_event_type), (Some(wire), Some(json)) if wire != json) {
        return EventClass::Malformed;
    }
    let Some(event_type) = json_event_type.or(wire_event_type) else {
        return EventClass::Malformed;
    };
    match target {
        Target::Codex => match event_type {
            "error" | "response.failed" | "response.incomplete" => EventClass::Failure,
            "response.created" | "response.in_progress" | "response.queued" => {
                EventClass::Lifecycle
            }
            "response.completed" => EventClass::NonFailureTerminal,
            event if event.starts_with("response.") => EventClass::Productive,
            _ => EventClass::Malformed,
        },
        Target::Claude => match event_type {
            "error" => EventClass::Failure,
            "message_start" | "ping" | "content_block_start" => EventClass::Lifecycle,
            "message_stop" => EventClass::NonFailureTerminal,
            "content_block_delta" | "content_block_stop" | "message_delta" => {
                EventClass::Productive
            }
            _ => EventClass::Malformed,
        },
    }
}

fn committed_with_prefix(mut response: UpstreamResponse, prefix: Vec<Bytes>) -> PrimedResponse {
    let body = stream::iter(prefix.into_iter().map(Ok::<_, UpstreamError>)).chain(response.body);
    response.body = Box::pin(body);
    PrimedResponse::Committed(response)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Bytes,
        http::{HeaderMap, HeaderValue, StatusCode, header},
    };
    use futures_util::stream;

    use super::{PrimedResponse, prime_success_response};
    use crate::{control::protocol::Target, model::UpstreamResponse};

    fn response(
        content_type: Option<&'static str>,
        chunks: Vec<&'static [u8]>,
    ) -> UpstreamResponse {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = content_type {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        }
        UpstreamResponse {
            status: StatusCode::OK,
            headers,
            body: Box::pin(stream::iter(
                chunks
                    .into_iter()
                    .map(|chunk| Ok(Bytes::from_static(chunk))),
            )),
        }
    }

    #[tokio::test]
    async fn lifecycle_then_error_is_retryable_but_productive_event_commits_exact_prefix() {
        let failed = response(
            Some("text/event-stream"),
            vec![
                b"data: {\"type\":\"response.created\"}\n\n",
                b"data: {\"type\":\"response.failed\"}\n\n",
            ],
        );
        assert!(matches!(
            prime_success_response(failed, Target::Codex).await,
            PrimedResponse::Retry
        ));

        let committed = response(
            Some("text/event-stream"),
            vec![
                b"data: {\"type\":\"message_start\"}\n\n",
                b"data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"ok\"}}\n\n",
            ],
        );
        assert!(matches!(
            prime_success_response(committed, Target::Claude).await,
            PrimedResponse::Committed(_)
        ));
    }

    #[tokio::test]
    async fn sse_comments_and_event_names_are_protocol_valid_before_commitment() {
        let committed = response(
            Some("text/event-stream"),
            vec![
                b": keepalive\n\n",
                b"event: content_block_delta\ndata: {\"delta\":{\"text\":\"ok\"}}\n\n",
            ],
        );
        assert!(matches!(
            prime_success_response(committed, Target::Claude).await,
            PrimedResponse::Committed(_)
        ));
    }

    #[tokio::test]
    async fn missing_content_type_and_json_error_are_semantic_failures() {
        assert!(matches!(
            prime_success_response(response(None, vec![b"{\"ok\":true}"]), Target::Codex).await,
            PrimedResponse::Retry
        ));
        assert!(matches!(
            prime_success_response(
                response(
                    Some("application/json"),
                    vec![b"{\"error\":{\"message\":\"no\"}}"]
                ),
                Target::Codex,
            )
            .await,
            PrimedResponse::Retry
        ));
    }
}
