use std::{
    io::Read,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use axum::{body::Bytes, http::StatusCode};
use flate2::read::MultiGzDecoder;
use futures_util::{StreamExt, stream};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    control::protocol::{ProviderProtocol, RequestRecordOutcome, Target},
    request_history::{PricingCatalog, RecordedProvider, RequestRecordCompletion, RequestUsage},
    state::StateStore,
};

use super::{UpstreamError, server::ActiveRequestGuard};

const COMPLETION_QUEUE_CAPACITY: usize = 64;
const MAX_USAGE_EVENT_BYTES: usize = 256 * 1024;
const MAX_ERROR_PAYLOAD_BYTES: usize = 65_536;
const MAX_COMPRESSED_OBSERVATION_BYTES: usize = 1024 * 1024;
const REDACTED: &[u8] = b"[REDACTED]";

#[derive(Clone)]
pub(crate) struct RequestRecorder {
    sender: mpsc::Sender<RequestRecordCompletion>,
}

pub(crate) struct RequestRecorderActor {
    receiver: mpsc::Receiver<RequestRecordCompletion>,
    store: Arc<StateStore>,
    catalog: PricingCatalog,
}

pub(crate) struct RequestRecordStart {
    pub(crate) id: uuid::Uuid,
    pub(crate) target: Target,
    pub(crate) plan_id: uuid::Uuid,
    pub(crate) plan_epoch: uuid::Uuid,
    pub(crate) provider: Option<RecordedProvider>,
    pub(crate) model: String,
    pub(crate) protocol: ProviderProtocol,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) status: Option<StatusCode>,
    pub(crate) semantic_failure: bool,
    pub(crate) observation: ResponseObservation,
}

#[derive(Clone, Copy)]
pub(crate) enum ResponseUsageFormat {
    CodexSse,
    CodexJson,
    ClaudeSse,
    ClaudeJson,
    Unsupported,
}

impl ResponseUsageFormat {
    pub(crate) fn codex(content_type: Option<&str>) -> Self {
        Self::for_content_type(content_type, Self::CodexSse, Self::CodexJson)
    }

    pub(crate) fn claude(content_type: Option<&str>) -> Self {
        Self::for_content_type(content_type, Self::ClaudeSse, Self::ClaudeJson)
    }

    fn for_content_type(content_type: Option<&str>, sse: Self, json: Self) -> Self {
        let normalized = content_type
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        match normalized {
            Some("text/event-stream") => sse,
            Some("application/json") => json,
            Some(value) if value.ends_with("+json") => json,
            _ => Self::Unsupported,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ResponseObservation {
    state: Arc<Mutex<ResponseObservationState>>,
}

struct ResponseObservationState {
    usage: UsageParser,
    capture_error_payload: bool,
    error_payload: Vec<u8>,
    error_payload_limit: usize,
    error_payload_truncated: bool,
    redactions: Vec<Vec<u8>>,
    gzip: bool,
    compressed: Vec<u8>,
    compressed_truncated: bool,
}

impl ResponseObservation {
    pub(crate) fn new(redactions: Vec<Vec<u8>>) -> Self {
        let error_payload_limit = MAX_ERROR_PAYLOAD_BYTES
            .saturating_add(redactions.iter().map(Vec::len).max().unwrap_or(0));
        Self {
            state: Arc::new(Mutex::new(ResponseObservationState {
                usage: UsageParser::Unsupported,
                capture_error_payload: false,
                error_payload: Vec::new(),
                error_payload_limit,
                error_payload_truncated: false,
                redactions,
                gzip: false,
                compressed: Vec::new(),
                compressed_truncated: false,
            })),
        }
    }

    pub(crate) fn configure(&self, format: ResponseUsageFormat, status: StatusCode, gzip: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.usage = UsageParser::new(format);
        state.capture_error_payload = !status.is_success();
        state.gzip = gzip;
    }

    pub(crate) fn add_redaction(&self, secret: &[u8]) {
        if secret.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.error_payload_limit = state
            .error_payload_limit
            .max(MAX_ERROR_PAYLOAD_BYTES.saturating_add(secret.len()));
        state.redactions.push(secret.to_vec());
    }

    pub(crate) fn observe(&self, bytes: &[u8]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.gzip {
            let remaining = MAX_COMPRESSED_OBSERVATION_BYTES.saturating_sub(state.compressed.len());
            state
                .compressed
                .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            if bytes.len() > remaining {
                state.compressed_truncated = true;
            }
            return;
        }
        if !state.capture_error_payload {
            state.usage.push(bytes);
            return;
        }
        let remaining = state
            .error_payload_limit
            .saturating_sub(state.error_payload.len());
        state
            .error_payload
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        if bytes.len() > remaining {
            state.error_payload_truncated = true;
        }
    }

    fn finish(
        &self,
        outcome: RequestRecordOutcome,
    ) -> (Option<RequestUsage>, Option<Vec<u8>>, bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.gzip && !state.compressed_truncated {
            let limit = if state.capture_error_payload {
                state.error_payload_limit
            } else {
                MAX_USAGE_EVENT_BYTES
            };
            let mut decoder =
                MultiGzDecoder::new(state.compressed.as_slice()).take((limit + 1) as u64);
            let mut decoded = Vec::new();
            if decoder.read_to_end(&mut decoded).is_ok() && decoded.len() <= limit {
                if state.capture_error_payload {
                    state.error_payload = decoded;
                } else {
                    state.usage.push(&decoded);
                }
            } else if state.capture_error_payload {
                state.error_payload.clear();
                state.error_payload_truncated = true;
            }
        } else if state.gzip && state.capture_error_payload {
            state.error_payload.clear();
            state.error_payload_truncated = true;
        }
        let usage = state.usage.finish();
        if outcome != RequestRecordOutcome::UpstreamError {
            return (usage, None, false);
        }
        let mut payload = redact(&state.error_payload, &state.redactions);
        if payload.len() > MAX_ERROR_PAYLOAD_BYTES {
            payload.truncate(MAX_ERROR_PAYLOAD_BYTES);
        }
        let truncated =
            state.error_payload_truncated || state.error_payload.len() > MAX_ERROR_PAYLOAD_BYTES;
        (usage, Some(payload), truncated)
    }
}

pub(crate) struct ReservedRequestRecord {
    permit: Option<mpsc::OwnedPermit<RequestRecordCompletion>>,
}

impl RequestRecorder {
    pub(crate) fn new(
        store: Arc<StateStore>,
    ) -> Result<(Self, RequestRecorderActor), crate::request_history::PricingError> {
        let catalog = PricingCatalog::release_pinned()?;
        let (sender, receiver) = mpsc::channel(COMPLETION_QUEUE_CAPACITY);
        Ok((
            Self { sender },
            RequestRecorderActor {
                receiver,
                store,
                catalog,
            },
        ))
    }

    pub(crate) fn reserve(&self) -> Option<ReservedRequestRecord> {
        self.sender
            .clone()
            .try_reserve_owned()
            .ok()
            .map(|permit| ReservedRequestRecord {
                permit: Some(permit),
            })
    }
}

impl RequestRecorderActor {
    pub(crate) async fn run(mut self) {
        while let Some(completion) = self.receiver.recv().await {
            if self
                .store
                .insert_request_record(completion, &self.catalog)
                .await
                .is_err()
            {
                eprintln!("request-record-storage-failed");
            }
        }
    }
}

impl ReservedRequestRecord {
    pub(crate) fn complete(mut self, completion: RequestRecordCompletion) {
        if let Some(permit) = self.permit.take() {
            permit.send(completion);
        }
    }

    pub(crate) fn complete_terminal(
        self,
        start: RequestRecordStart,
        outcome: RequestRecordOutcome,
    ) {
        self.complete(RequestRecordCompletion {
            id: start.id,
            target: start.target,
            plan_id: start.plan_id,
            plan_epoch: start.plan_epoch,
            provider: start.provider,
            model: start.model,
            protocol: start.protocol,
            started_at_unix_ms: start.started_at_unix_ms,
            finished_at_unix_ms: unix_time_ms().max(start.started_at_unix_ms),
            outcome,
            http_status: start.status.map(|status| status.as_u16()),
            usage: None,
            error_payload: None,
            error_payload_truncated: false,
        });
    }
}

struct StreamingRecord {
    reserved: Option<ReservedRequestRecord>,
    start: Option<RequestRecordStart>,
    terminated: bool,
}

impl StreamingRecord {
    fn new(reserved: ReservedRequestRecord, start: RequestRecordStart) -> Self {
        Self {
            reserved: Some(reserved),
            start: Some(start),
            terminated: false,
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        if let Some(start) = &self.start {
            start.observation.observe(bytes);
        }
    }

    fn finish(&mut self, outcome: Option<RequestRecordOutcome>) {
        let Some(reserved) = self.reserved.take() else {
            return;
        };
        let start = self.start.take().expect("record start accompanies permit");
        let outcome = outcome.unwrap_or_else(|| match start.status {
            Some(status) if !status.is_success() => RequestRecordOutcome::UpstreamError,
            Some(_) if start.semantic_failure => RequestRecordOutcome::SemanticError,
            Some(_) => RequestRecordOutcome::Success,
            None => RequestRecordOutcome::RouteUnavailable,
        });
        let (usage, error_payload, error_payload_truncated) = start.observation.finish(outcome);
        reserved.complete(RequestRecordCompletion {
            id: start.id,
            target: start.target,
            plan_id: start.plan_id,
            plan_epoch: start.plan_epoch,
            provider: start.provider,
            model: start.model,
            protocol: start.protocol,
            started_at_unix_ms: start.started_at_unix_ms,
            finished_at_unix_ms: unix_time_ms().max(start.started_at_unix_ms),
            outcome,
            http_status: start.status.map(|status| status.as_u16()),
            usage,
            error_payload,
            error_payload_truncated,
        });
        self.terminated = true;
    }
}

impl Drop for StreamingRecord {
    fn drop(&mut self) {
        if self.reserved.is_some() {
            self.finish(Some(RequestRecordOutcome::Cancelled));
        }
    }
}

pub(crate) fn recorded_body(
    body: std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, UpstreamError>> + Send>>,
    active_request: ActiveRequestGuard,
    reserved: ReservedRequestRecord,
    start: RequestRecordStart,
    observe_forwarded: bool,
) -> axum::body::Body {
    let recorded = stream::unfold(
        (
            body,
            StreamingRecord::new(reserved, start),
            Some(active_request),
        ),
        move |(mut body, mut record, active_request)| async move {
            if record.terminated {
                return None;
            }
            match body.next().await {
                Some(Ok(bytes)) => {
                    if observe_forwarded {
                        record.observe(&bytes);
                    }
                    Some((Ok(bytes), (body, record, active_request)))
                }
                Some(Err(error)) => {
                    record.finish(Some(RequestRecordOutcome::StreamError));
                    Some((Err(error), (body, record, active_request)))
                }
                None => {
                    record.finish(None);
                    None
                }
            }
        },
    );
    axum::body::Body::from_stream(recorded)
}

pub(crate) fn observed_upstream_body(
    body: std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, UpstreamError>> + Send>>,
    observation: ResponseObservation,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, UpstreamError>> + Send>> {
    Box::pin(stream::unfold(
        (body, observation),
        |(mut body, observation)| async move {
            body.next().await.map(|item| {
                if let Ok(bytes) = &item {
                    observation.observe(bytes);
                }
                (item, (body, observation))
            })
        },
    ))
}

enum UsageParser {
    Codex(CodexUsageParser),
    Claude(ClaudeUsageParser),
    Unsupported,
}

impl UsageParser {
    fn new(format: ResponseUsageFormat) -> Self {
        match format {
            ResponseUsageFormat::CodexSse => Self::Codex(CodexUsageParser::new(StreamFormat::Sse)),
            ResponseUsageFormat::CodexJson => {
                Self::Codex(CodexUsageParser::new(StreamFormat::Json))
            }
            ResponseUsageFormat::ClaudeSse => {
                Self::Claude(ClaudeUsageParser::new(StreamFormat::Sse))
            }
            ResponseUsageFormat::ClaudeJson => {
                Self::Claude(ClaudeUsageParser::new(StreamFormat::Json))
            }
            ResponseUsageFormat::Unsupported => Self::Unsupported,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        match self {
            Self::Codex(parser) => parser.push(bytes),
            Self::Claude(parser) => parser.push(bytes),
            Self::Unsupported => {}
        }
    }

    fn finish(&mut self) -> Option<RequestUsage> {
        match self {
            Self::Codex(parser) => parser.finish(),
            Self::Claude(parser) => parser.finish(),
            Self::Unsupported => None,
        }
    }
}

#[derive(Clone, Copy)]
enum StreamFormat {
    Sse,
    Json,
}

struct CodexUsageParser {
    format: StreamFormat,
    pending: Vec<u8>,
    usage: Option<RequestUsage>,
    disabled: bool,
}

impl CodexUsageParser {
    fn new(format: StreamFormat) -> Self {
        Self {
            format,
            pending: Vec::new(),
            usage: None,
            disabled: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.disabled {
            return;
        }
        if self.pending.len().saturating_add(bytes.len()) > MAX_USAGE_EVENT_BYTES {
            self.pending.clear();
            self.disabled = true;
            return;
        }
        self.pending.extend_from_slice(bytes);
        if !matches!(self.format, StreamFormat::Sse) {
            return;
        }
        while let Some(end) = self.pending.windows(2).position(|window| window == b"\n\n") {
            let event = self.pending.drain(..end + 2).collect::<Vec<_>>();
            self.observe_event(&event);
        }
    }

    fn observe_event(&mut self, event: &[u8]) {
        if let Some(value) = sse_value(event) {
            self.observe_value(&value);
        }
    }

    fn observe_value(&mut self, value: &Value) {
        let usage = value
            .get("response")
            .and_then(|response| response.get("usage"))
            .or_else(|| value.get("usage"));
        let Some(usage) = usage else {
            return;
        };
        let input = usage.get("input_tokens").and_then(Value::as_u64);
        let output = usage.get("output_tokens").and_then(Value::as_u64);
        let (Some(input), Some(output)) = (input, output) else {
            return;
        };
        let details = usage.get("input_tokens_details");
        let cached = usage
            .get("cache_read_input_tokens")
            .or_else(|| details.and_then(|value| value.get("cached_tokens")))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(input);
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .or_else(|| details.and_then(|value| value.get("cache_write_tokens")))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(input.saturating_sub(cached));
        self.usage = Some(RequestUsage {
            input_tokens: input.saturating_sub(cached).saturating_sub(cache_creation),
            cached_input_tokens: cached,
            cache_creation_input_tokens: cache_creation,
            output_tokens: output,
        });
    }

    fn finish(&mut self) -> Option<RequestUsage> {
        if !self.disabled
            && matches!(self.format, StreamFormat::Json)
            && let Ok(value) = serde_json::from_slice::<Value>(&self.pending)
        {
            self.observe_value(&value);
        }
        self.usage
    }
}

struct ClaudeUsageParser {
    format: StreamFormat,
    pending: Vec<u8>,
    usage: RequestUsage,
    observed: bool,
    disabled: bool,
}

impl ClaudeUsageParser {
    fn new(format: StreamFormat) -> Self {
        Self {
            format,
            pending: Vec::new(),
            usage: RequestUsage {
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                output_tokens: 0,
            },
            observed: false,
            disabled: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.disabled {
            return;
        }
        if self.pending.len().saturating_add(bytes.len()) > MAX_USAGE_EVENT_BYTES {
            self.pending.clear();
            self.disabled = true;
            return;
        }
        self.pending.extend_from_slice(bytes);
        if !matches!(self.format, StreamFormat::Sse) {
            return;
        }
        while let Some(end) = self.pending.windows(2).position(|window| window == b"\n\n") {
            let event = self.pending.drain(..end + 2).collect::<Vec<_>>();
            if let Some(value) = sse_value(&event) {
                self.observe_value(&value);
            }
        }
    }

    fn observe_value(&mut self, value: &Value) {
        let usage = value
            .get("message")
            .and_then(|message| message.get("usage"))
            .or_else(|| value.get("usage"));
        let Some(usage) = usage else {
            return;
        };
        if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.usage.input_tokens = input;
            self.observed = true;
        }
        if let Some(cached) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.usage.cached_input_tokens = cached;
            self.observed = true;
        }
        if let Some(creation) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.usage.cache_creation_input_tokens = creation;
            self.observed = true;
        }
        if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.usage.output_tokens = output;
            self.observed = true;
        }
    }

    fn finish(&mut self) -> Option<RequestUsage> {
        if !self.disabled
            && matches!(self.format, StreamFormat::Json)
            && let Ok(value) = serde_json::from_slice::<Value>(&self.pending)
        {
            self.observe_value(&value);
        }
        self.observed.then_some(self.usage)
    }
}

fn sse_value(event: &[u8]) -> Option<Value> {
    let mut data = Vec::new();
    for line in event.split(|byte| *byte == b'\n') {
        let Some(value) = line.strip_prefix(b"data:") else {
            continue;
        };
        if !data.is_empty() {
            data.push(b'\n');
        }
        data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
    }
    serde_json::from_slice(&data).ok()
}

pub(crate) fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn redact(input: &[u8], secrets: &[Vec<u8>]) -> Vec<u8> {
    let mut redacted = input.to_vec();
    let mut ordered = secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    ordered.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in ordered {
        let replacement = if secret.len() >= REDACTED.len() {
            let mut replacement = REDACTED.to_vec();
            replacement.resize(secret.len(), b'*');
            replacement
        } else {
            vec![b'*'; secret.len()]
        };
        let mut offset = 0;
        while let Some(relative) = redacted[offset..]
            .windows(secret.len())
            .position(|window| window == secret.as_slice())
        {
            let start = offset + relative;
            redacted.splice(start..start + secret.len(), replacement.iter().copied());
            offset = start + replacement.len();
        }
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::MuxviaHome;
    use tempfile::TempDir;

    #[test]
    fn codex_usage_parser_handles_fragmented_sse_and_complete_json() {
        let mut sse = CodexUsageParser::new(StreamFormat::Sse);
        for fragment in [
            b"data: {\"type\":\"response.completed\",\"response\":{\"usa".as_slice(),
            b"ge\":{\"input_tokens\":20,\"input_tokens_details\":{\"cached_tokens\":5},".as_slice(),
            b"\"output_tokens\":7}}}\n\n".as_slice(),
        ] {
            sse.push(fragment);
        }
        assert_eq!(
            sse.finish(),
            Some(RequestUsage {
                input_tokens: 15,
                cached_input_tokens: 5,
                cache_creation_input_tokens: 0,
                output_tokens: 7,
            })
        );

        let mut json = CodexUsageParser::new(StreamFormat::Json);
        json.push(br#"{"id":"response","usage":{"input_tokens":9,"output_tokens":4}}"#);
        assert_eq!(
            json.finish(),
            Some(RequestUsage {
                input_tokens: 9,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                output_tokens: 4,
            })
        );
    }

    #[test]
    fn claude_usage_parser_merges_message_start_and_delta_without_generic_wire_state() {
        let mut parser = ClaudeUsageParser::new(StreamFormat::Sse);
        let bytes = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":0,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":1}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n\n";
        for fragment in bytes.chunks(17) {
            parser.push(fragment);
        }
        assert_eq!(
            parser.finish(),
            Some(RequestUsage {
                input_tokens: 12,
                cached_input_tokens: 2,
                cache_creation_input_tokens: 1,
                output_tokens: 4,
            })
        );
    }

    #[test]
    fn usage_parser_discards_oversized_state_instead_of_retaining_a_success_body() {
        let mut parser = CodexUsageParser::new(StreamFormat::Json);
        parser.push(&vec![b'x'; MAX_USAGE_EVENT_BYTES + 1]);
        assert!(parser.finish().is_none());
        assert!(parser.pending.is_empty());
        assert!(parser.disabled);
    }

    #[tokio::test]
    async fn recorder_reserves_exactly_the_bounded_completion_capacity() {
        let home = TempDir::new().unwrap();
        let store = Arc::new(
            StateStore::open(&MuxviaHome::from_user_home(home.path()))
                .await
                .unwrap(),
        );
        let (recorder, _actor) = RequestRecorder::new(store).unwrap();
        let mut reservations = Vec::new();
        for _ in 0..COMPLETION_QUEUE_CAPACITY {
            reservations.push(recorder.reserve().unwrap());
        }
        assert!(
            recorder.reserve().is_none(),
            "request recorder admitted more than its bounded capacity"
        );
        reservations.pop();
        assert!(
            recorder.reserve().is_some(),
            "request recorder did not release dropped capacity"
        );
    }
}
