use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt,
};

use axum::{
    body::Bytes,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
};
use futures_util::{Stream, StreamExt, stream};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value, json};

pub(crate) const SUBSCRIPTION_BRIDGE_RESPONSES_URL: &str =
    "https://chatgpt.com/backend-api/codex/responses";
pub(crate) const MAX_BRIDGE_BODY_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_BRIDGE_SSE_EVENT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_BRIDGE_ERROR_BODY_BYTES: usize = 256 * 1024;

const ORIGINATOR: &str = "codex_cli_rs";
const CODEX_VERSION: &str = "0.144.1";
const TOOL_RESULT_ERROR_MARKER: &str = "[cc-switch:tool-result-error]";

pub(crate) struct BridgeRequestInput<'a> {
    pub(crate) path: &'a str,
    pub(crate) provider_model: &'a str,
    pub(crate) account_id: &'a str,
    pub(crate) access_token: &'a SecretString,
    pub(crate) inbound_headers: &'a HeaderMap,
    pub(crate) body: &'a [u8],
}

pub(crate) struct PreparedBridgeRequest {
    headers: HeaderMap,
    body: Value,
}

impl PreparedBridgeRequest {
    pub(crate) fn url(&self) -> &'static str {
        SUBSCRIPTION_BRIDGE_RESPONSES_URL
    }

    pub(crate) fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub(crate) fn body(&self) -> &Value {
        &self.body
    }

    #[cfg(test)]
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }
}

impl fmt::Debug for PreparedBridgeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedBridgeRequest")
            .field("url", &SUBSCRIPTION_BRIDGE_RESPONSES_URL)
            .field("headers", &"[redacted]")
            .field("body", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum BridgeRequestError {
    #[error("subscription bridge request is invalid")]
    InvalidRequest,
    #[error("subscription bridge request is too large")]
    RequestTooLarge,
    #[error("subscription bridge token counting is unsupported")]
    CountTokensUnsupported,
}

pub(crate) struct SubscriptionBridgeAdapter;

impl SubscriptionBridgeAdapter {
    pub(crate) fn prepare(
        input: BridgeRequestInput<'_>,
    ) -> Result<PreparedBridgeRequest, BridgeRequestError> {
        if input.path.ends_with("/count_tokens") {
            return Err(BridgeRequestError::CountTokensUnsupported);
        }
        if input.path != "/v1/messages" {
            return Err(BridgeRequestError::InvalidRequest);
        }
        if input.body.len() > MAX_BRIDGE_BODY_BYTES {
            return Err(BridgeRequestError::RequestTooLarge);
        }
        let source = serde_json::from_slice::<Value>(input.body)
            .map_err(|_| BridgeRequestError::InvalidRequest)?;
        let source = source
            .as_object()
            .ok_or(BridgeRequestError::InvalidRequest)?;
        let body = transform_request(source, input.provider_model)?;
        let session = session_identity(source, input.inbound_headers)?;
        let headers = identity_headers(
            input.account_id,
            input.access_token.expose_secret(),
            session.as_deref(),
        )?;

        Ok(PreparedBridgeRequest { headers, body })
    }

    pub(crate) fn convert_stream<S, E>(upstream: S) -> impl Stream<Item = Bytes> + Send
    where
        S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
        E: Send + 'static,
    {
        stream::unfold(BridgeSseState::new(upstream), |mut state| async move {
            let output = state.next_output().await?;
            Some((output, state))
        })
    }

    pub(crate) fn map_non_success(
        status: StatusCode,
        body: &[u8],
    ) -> Result<PreparedBridgeFailure, BridgeResponseError> {
        if status.is_success() || body.len() > MAX_BRIDGE_ERROR_BODY_BYTES {
            return Err(BridgeResponseError::InvalidResponse);
        }
        Ok(PreparedBridgeFailure {
            status,
            event: bridge_error_event("subscription-bridge-upstream-error"),
        })
    }

    pub(crate) fn invalid_response(status: StatusCode) -> PreparedBridgeFailure {
        PreparedBridgeFailure {
            status,
            event: bridge_error_event("subscription-bridge-invalid-response"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum BridgeResponseError {
    #[error("subscription bridge response is invalid")]
    InvalidResponse,
}

pub(crate) struct PreparedBridgeFailure {
    status: StatusCode,
    event: Bytes,
}

impl PreparedBridgeFailure {
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn event(&self) -> &Bytes {
        &self.event
    }
}

impl fmt::Debug for PreparedBridgeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedBridgeFailure")
            .field("status", &self.status)
            .field("event", &"[redacted]")
            .finish()
    }
}

struct ToolBlock {
    index: u64,
    arguments: String,
    had_delta: bool,
}

struct ReasoningBlock {
    index: u64,
    open: bool,
}

struct BridgeSseState<S> {
    upstream: S,
    buffer: Vec<u8>,
    output: VecDeque<Bytes>,
    terminated: bool,
    message_started: bool,
    next_index: u64,
    text_index: Option<u64>,
    tools: HashMap<String, ToolBlock>,
    reasoning: HashMap<String, ReasoningBlock>,
    has_tool_use: bool,
}

impl<S, E> BridgeSseState<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    fn new(upstream: S) -> Self {
        Self {
            upstream,
            buffer: Vec::new(),
            output: VecDeque::new(),
            terminated: false,
            message_started: false,
            next_index: 0,
            text_index: None,
            tools: HashMap::new(),
            reasoning: HashMap::new(),
            has_tool_use: false,
        }
    }

    async fn next_output(&mut self) -> Option<Bytes> {
        loop {
            if let Some(output) = self.output.pop_front() {
                return Some(output);
            }
            if self.terminated {
                return None;
            }
            match self.upstream.next().await {
                Some(Ok(chunk)) => {
                    self.buffer.extend_from_slice(&chunk);
                    self.process_buffer();
                    if !self.terminated && self.buffer.len() > MAX_BRIDGE_SSE_EVENT_BYTES {
                        self.fail_invalid();
                    }
                }
                Some(Err(_)) => self.fail_invalid(),
                None => self.fail_invalid(),
            }
        }
    }

    fn process_buffer(&mut self) {
        while !self.terminated {
            let Some(block) = take_sse_block(&mut self.buffer) else {
                break;
            };
            if block.len() > MAX_BRIDGE_SSE_EVENT_BYTES || self.process_block(&block).is_err() {
                self.fail_invalid();
            }
        }
    }

    fn process_block(&mut self, block: &[u8]) -> Result<(), BridgeResponseError> {
        let block = std::str::from_utf8(block).map_err(|_| BridgeResponseError::InvalidResponse)?;
        let mut event_name = None;
        let mut data = Vec::new();
        for line in block.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.starts_with(':') || line.is_empty() {
                continue;
            }
            if let Some(value) = line.strip_prefix("event:") {
                event_name = Some(value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("data:") {
                data.push(value.strip_prefix(' ').unwrap_or(value));
            }
        }
        if data.is_empty() {
            return Ok(());
        }
        let payload = serde_json::from_str::<Value>(&data.join("\n"))
            .map_err(|_| BridgeResponseError::InvalidResponse)?;
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .ok_or(BridgeResponseError::InvalidResponse)?;
        if event_name
            .as_deref()
            .is_some_and(|event_name| event_name != payload_type)
        {
            return Err(BridgeResponseError::InvalidResponse);
        }
        self.process_event(payload_type, &payload)
    }

    fn process_event(
        &mut self,
        event_name: &str,
        payload: &Value,
    ) -> Result<(), BridgeResponseError> {
        match event_name {
            "response.created" => self.start_message(payload),
            "response.content_part.added" => self.start_text(payload),
            "response.output_text.delta" | "response.refusal.delta" => self.text_delta(payload),
            "response.output_text.done" | "response.refusal.done" => self.stop_text(),
            "response.content_part.done" => Ok(()),
            "response.output_item.added" => self.start_output_item(payload),
            "response.function_call_arguments.delta" => self.tool_arguments_delta(payload),
            "response.function_call_arguments.done" => self.stop_tool(payload),
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning.delta" => self.reasoning_delta(payload),
            "response.output_item.done" => self.stop_output_item(payload),
            "response.completed" | "response.incomplete" => {
                self.complete_message(event_name, payload)
            }
            "response.failed" | "error" => {
                self.output
                    .push_back(bridge_error_event("subscription-bridge-upstream-error"));
                self.terminated = true;
                Ok(())
            }
            _ => Err(BridgeResponseError::InvalidResponse),
        }
    }

    fn start_message(&mut self, payload: &Value) -> Result<(), BridgeResponseError> {
        if self.message_started {
            return Err(BridgeResponseError::InvalidResponse);
        }
        let response = response_object(payload);
        let id = required_response_string(response, "id")?;
        let model = required_response_string(response, "model")?;
        let mut usage = response_usage(response.get("usage"))?;
        usage["output_tokens"] = Value::Number(0.into());
        self.output.push_back(anthropic_event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "usage": usage
                }
            }),
        ));
        self.message_started = true;
        Ok(())
    }

    fn start_text(&mut self, payload: &Value) -> Result<(), BridgeResponseError> {
        self.require_started()?;
        let part_type = payload
            .get("part")
            .and_then(|part| part.get("type"))
            .and_then(Value::as_str)
            .ok_or(BridgeResponseError::InvalidResponse)?;
        if !matches!(part_type, "output_text" | "refusal") {
            return Err(BridgeResponseError::InvalidResponse);
        }
        if self.text_index.is_some() {
            return Ok(());
        }
        let index = self.allocate_index();
        self.text_index = Some(index);
        self.output.push_back(anthropic_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
        Ok(())
    }

    fn text_delta(&mut self, payload: &Value) -> Result<(), BridgeResponseError> {
        self.require_started()?;
        let index = self
            .text_index
            .ok_or(BridgeResponseError::InvalidResponse)?;
        let delta = required_response_string(payload, "delta")?;
        self.output.push_back(anthropic_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": delta}
            }),
        ));
        Ok(())
    }

    fn stop_text(&mut self) -> Result<(), BridgeResponseError> {
        let index = self
            .text_index
            .take()
            .ok_or(BridgeResponseError::InvalidResponse)?;
        self.push_block_stop(index);
        Ok(())
    }

    fn start_output_item(&mut self, payload: &Value) -> Result<(), BridgeResponseError> {
        self.require_started()?;
        let item = payload
            .get("item")
            .and_then(Value::as_object)
            .ok_or(BridgeResponseError::InvalidResponse)?;
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                if let Some(index) = self.text_index.take() {
                    self.push_block_stop(index);
                }
                let item_id = required_response_object_string(item, "id")?.to_owned();
                if self.tools.contains_key(&item_id) {
                    return Err(BridgeResponseError::InvalidResponse);
                }
                let call_id = required_response_object_string(item, "call_id")?;
                let name = required_response_object_string(item, "name")?;
                let index = self.allocate_index();
                self.tools.insert(
                    item_id,
                    ToolBlock {
                        index,
                        arguments: String::new(),
                        had_delta: false,
                    },
                );
                self.has_tool_use = true;
                self.output.push_back(anthropic_event(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "tool_use", "id": call_id, "name": name}
                    }),
                ));
                Ok(())
            }
            Some("reasoning") => {
                let item_id = required_response_object_string(item, "id")?.to_owned();
                if self.reasoning.contains_key(&item_id) {
                    return Err(BridgeResponseError::InvalidResponse);
                }
                let index = self.allocate_index();
                self.reasoning
                    .insert(item_id, ReasoningBlock { index, open: false });
                Ok(())
            }
            Some("message") => Ok(()),
            _ => Err(BridgeResponseError::InvalidResponse),
        }
    }

    fn tool_arguments_delta(&mut self, payload: &Value) -> Result<(), BridgeResponseError> {
        self.require_started()?;
        let item_id = required_response_string(payload, "item_id")?;
        let delta = required_response_string(payload, "delta")?;
        let tool = self
            .tools
            .get_mut(item_id)
            .ok_or(BridgeResponseError::InvalidResponse)?;
        tool.arguments.push_str(delta);
        tool.had_delta = true;
        self.output.push_back(anthropic_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": tool.index,
                "delta": {"type": "input_json_delta", "partial_json": delta}
            }),
        ));
        Ok(())
    }

    fn stop_tool(&mut self, payload: &Value) -> Result<(), BridgeResponseError> {
        let item_id = required_response_string(payload, "item_id")?;
        let mut tool = self
            .tools
            .remove(item_id)
            .ok_or(BridgeResponseError::InvalidResponse)?;
        let complete = payload
            .get("arguments")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| tool.arguments.clone());
        serde_json::from_str::<Value>(&complete)
            .map_err(|_| BridgeResponseError::InvalidResponse)?;
        if tool.had_delta && complete != tool.arguments {
            return Err(BridgeResponseError::InvalidResponse);
        }
        if !tool.had_delta {
            tool.arguments.push_str(&complete);
            self.output.push_back(anthropic_event(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": tool.index,
                    "delta": {"type": "input_json_delta", "partial_json": complete}
                }),
            ));
        }
        self.push_block_stop(tool.index);
        Ok(())
    }

    fn reasoning_delta(&mut self, payload: &Value) -> Result<(), BridgeResponseError> {
        self.require_started()?;
        if let Some(index) = self.text_index.take() {
            self.push_block_stop(index);
        }
        let item_id = required_response_string(payload, "item_id")?;
        let reasoning = self
            .reasoning
            .get_mut(item_id)
            .ok_or(BridgeResponseError::InvalidResponse)?;
        let delta = required_response_string(payload, "delta")?;
        if !reasoning.open {
            self.output.push_back(anthropic_event(
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "index": reasoning.index,
                    "content_block": {"type": "thinking", "thinking": ""}
                }),
            ));
            reasoning.open = true;
        }
        self.output.push_back(anthropic_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": reasoning.index,
                "delta": {"type": "thinking_delta", "thinking": delta}
            }),
        ));
        Ok(())
    }

    fn stop_output_item(&mut self, payload: &Value) -> Result<(), BridgeResponseError> {
        let item = payload
            .get("item")
            .and_then(Value::as_object)
            .ok_or(BridgeResponseError::InvalidResponse)?;
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                let item_id = required_response_object_string(item, "id")?;
                let reasoning = self
                    .reasoning
                    .remove(item_id)
                    .ok_or(BridgeResponseError::InvalidResponse)?;
                if reasoning.open {
                    self.push_block_stop(reasoning.index);
                }
                Ok(())
            }
            Some("function_call") => {
                let item_id = required_response_object_string(item, "id")?;
                if !self.tools.contains_key(item_id) {
                    return Ok(());
                }
                let mut done = payload.clone();
                done["item_id"] = Value::String(item_id.to_owned());
                if done.get("arguments").is_none() {
                    done["arguments"] = item
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| Value::String(String::new()));
                }
                self.stop_tool(&done)
            }
            Some("message") => Ok(()),
            _ => Err(BridgeResponseError::InvalidResponse),
        }
    }

    fn complete_message(
        &mut self,
        event_name: &str,
        payload: &Value,
    ) -> Result<(), BridgeResponseError> {
        self.require_started()?;
        let response = response_object(payload);
        if response.get("error").is_some_and(|error| !error.is_null())
            || matches!(
                response.get("status").and_then(Value::as_str),
                Some("failed" | "cancelled")
            )
        {
            self.output
                .push_back(bridge_error_event("subscription-bridge-upstream-error"));
            self.terminated = true;
            return Ok(());
        }
        if let Some(index) = self.text_index.take() {
            self.push_block_stop(index);
        }
        if !self.tools.is_empty() || !self.reasoning.is_empty() {
            return Err(BridgeResponseError::InvalidResponse);
        }
        let status = response.get("status").and_then(Value::as_str).unwrap_or(
            if event_name == "response.incomplete" {
                "incomplete"
            } else {
                "completed"
            },
        );
        if !matches!(status, "completed" | "incomplete") {
            return Err(BridgeResponseError::InvalidResponse);
        }
        let stop_reason = if status == "incomplete" {
            match response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
            {
                None | Some("max_output_tokens" | "max_tokens") => "max_tokens",
                Some(_) => "end_turn",
            }
        } else if self.has_tool_use {
            "tool_use"
        } else {
            "end_turn"
        };
        let usage = response_usage(response.get("usage"))?;
        self.output.push_back(anthropic_event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": usage
            }),
        ));
        self.output.push_back(anthropic_event(
            "message_stop",
            json!({"type": "message_stop"}),
        ));
        self.terminated = true;
        Ok(())
    }

    fn allocate_index(&mut self) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn require_started(&self) -> Result<(), BridgeResponseError> {
        if self.message_started {
            Ok(())
        } else {
            Err(BridgeResponseError::InvalidResponse)
        }
    }

    fn push_block_stop(&mut self, index: u64) {
        self.output.push_back(anthropic_event(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        ));
    }

    fn fail_invalid(&mut self) {
        if !self.terminated {
            self.output
                .push_back(bridge_error_event("subscription-bridge-invalid-response"));
            self.terminated = true;
        }
    }
}

fn take_sse_block(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    let (end, delimiter) = match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf <= crlf => (lf, 2),
        (Some(_), Some(crlf)) => (crlf, 4),
        (Some(lf), None) => (lf, 2),
        (None, Some(crlf)) => (crlf, 4),
        (None, None) => return None,
    };
    let block = buffer[..end].to_vec();
    buffer.drain(..end + delimiter);
    Some(block)
}

fn response_object(payload: &Value) -> &Value {
    payload.get("response").unwrap_or(payload)
}

fn required_response_string<'a>(
    value: &'a Value,
    field: &str,
) -> Result<&'a str, BridgeResponseError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(BridgeResponseError::InvalidResponse)
}

fn required_response_object_string<'a>(
    value: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, BridgeResponseError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(BridgeResponseError::InvalidResponse)
}

fn response_usage(usage: Option<&Value>) -> Result<Value, BridgeResponseError> {
    let usage = match usage {
        None | Some(Value::Null) => None,
        Some(Value::Object(usage)) => Some(usage),
        Some(_) => return Err(BridgeResponseError::InvalidResponse),
    };
    let read = usage
        .and_then(|usage| usage.get("cache_read_input_tokens"))
        .or_else(|| {
            usage
                .and_then(|usage| usage.get("input_tokens_details"))
                .and_then(|details| details.get("cached_tokens"))
        })
        .map(valid_usage_number)
        .transpose()?
        .unwrap_or(0);
    let creation = usage
        .and_then(|usage| usage.get("cache_creation_input_tokens"))
        .or_else(|| {
            usage
                .and_then(|usage| usage.get("input_tokens_details"))
                .and_then(|details| details.get("cache_write_tokens"))
        })
        .map(valid_usage_number)
        .transpose()?
        .unwrap_or(0);
    let total_input = usage
        .and_then(|usage| usage.get("input_tokens"))
        .map(valid_usage_number)
        .transpose()?
        .unwrap_or(0);
    let output = usage
        .and_then(|usage| usage.get("output_tokens"))
        .map(valid_usage_number)
        .transpose()?
        .unwrap_or(0);
    let mut result = json!({
        "input_tokens": total_input.saturating_sub(read).saturating_sub(creation),
        "output_tokens": output
    });
    if read > 0 {
        result["cache_read_input_tokens"] = Value::Number(read.into());
    }
    if creation > 0 {
        result["cache_creation_input_tokens"] = Value::Number(creation.into());
    }
    Ok(result)
}

fn valid_usage_number(value: &Value) -> Result<u64, BridgeResponseError> {
    value.as_u64().ok_or(BridgeResponseError::InvalidResponse)
}

fn anthropic_event(name: &str, payload: Value) -> Bytes {
    Bytes::from(format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(&payload).unwrap_or_default()
    ))
}

fn bridge_error_event(message: &str) -> Bytes {
    anthropic_event(
        "error",
        json!({
            "type": "error",
            "error": {"type": "api_error", "message": message}
        }),
    )
}

fn transform_request(
    source: &Map<String, Value>,
    provider_model: &str,
) -> Result<Value, BridgeRequestError> {
    let instructions = instructions(source.get("system"))?;
    let messages = source
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(BridgeRequestError::InvalidRequest)?;
    let input = convert_messages(messages)?;
    let tools = convert_tools(source.get("tools"))?;

    let mut result = Map::new();
    result.insert("model".to_owned(), Value::String(provider_model.to_owned()));
    result.insert("instructions".to_owned(), Value::String(instructions));
    result.insert("input".to_owned(), Value::Array(input));
    result.insert("store".to_owned(), Value::Bool(false));
    result.insert(
        "include".to_owned(),
        Value::Array(vec![Value::String(
            "reasoning.encrypted_content".to_owned(),
        )]),
    );
    result.insert("tools".to_owned(), Value::Array(tools));
    if let Some(tool_choice) = source.get("tool_choice") {
        result.insert("tool_choice".to_owned(), convert_tool_choice(tool_choice)?);
    }
    result.insert("parallel_tool_calls".to_owned(), Value::Bool(false));
    result.insert("stream".to_owned(), Value::Bool(true));
    Ok(Value::Object(result))
}

fn instructions(system: Option<&Value>) -> Result<String, BridgeRequestError> {
    match system {
        None => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| {
                let block = block
                    .as_object()
                    .ok_or(BridgeRequestError::InvalidRequest)?;
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(BridgeRequestError::InvalidRequest);
                }
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or(BridgeRequestError::InvalidRequest)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| parts.join("\n\n")),
        Some(_) => Err(BridgeRequestError::InvalidRequest),
    }
}

fn convert_messages(messages: &[Value]) -> Result<Vec<Value>, BridgeRequestError> {
    let mut input = Vec::new();
    for message in messages {
        let message = message
            .as_object()
            .ok_or(BridgeRequestError::InvalidRequest)?;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or(BridgeRequestError::InvalidRequest)?;
        if !matches!(role, "user" | "assistant") {
            return Err(BridgeRequestError::InvalidRequest);
        }
        let content = message
            .get("content")
            .ok_or(BridgeRequestError::InvalidRequest)?;
        match content {
            Value::String(text) => input.push(message_item(role, vec![text_part(role, text)])),
            Value::Array(blocks) => convert_content_blocks(role, blocks, &mut input)?,
            _ => return Err(BridgeRequestError::InvalidRequest),
        }
    }
    Ok(input)
}

fn convert_content_blocks(
    role: &str,
    blocks: &[Value],
    input: &mut Vec<Value>,
) -> Result<(), BridgeRequestError> {
    let mut content = Vec::new();
    for block in blocks {
        let block = block
            .as_object()
            .ok_or(BridgeRequestError::InvalidRequest)?;
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(BridgeRequestError::InvalidRequest)?;
                content.push(text_part(role, text));
            }
            Some("tool_use") if role == "assistant" => {
                flush_message(role, &mut content, input);
                let id = required_nonempty_string(block, "id")?;
                let name = required_nonempty_string(block, "name")?;
                let arguments = block
                    .get("input")
                    .ok_or(BridgeRequestError::InvalidRequest)?;
                input.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": canonical_json_string(arguments)?
                }));
            }
            Some("tool_result") if role == "user" => {
                flush_message(role, &mut content, input);
                let call_id = required_nonempty_string(block, "tool_use_id")?;
                let output = tool_result_output(block)?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output
                }));
            }
            _ => return Err(BridgeRequestError::InvalidRequest),
        }
    }
    flush_message(role, &mut content, input);
    Ok(())
}

fn required_nonempty_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, BridgeRequestError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(BridgeRequestError::InvalidRequest)
}

fn text_part(role: &str, text: &str) -> Value {
    let kind = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({"type": kind, "text": text})
}

fn message_item(role: &str, content: Vec<Value>) -> Value {
    json!({"role": role, "content": content})
}

fn flush_message(role: &str, content: &mut Vec<Value>, input: &mut Vec<Value>) {
    if !content.is_empty() {
        input.push(message_item(role, std::mem::take(content)));
    }
}

fn tool_result_output(block: &Map<String, Value>) -> Result<Value, BridgeRequestError> {
    let content = block
        .get("content")
        .ok_or(BridgeRequestError::InvalidRequest)?;
    let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
    if !is_error && let Value::String(_) = content {
        return Ok(content.clone());
    }

    let mut output = Vec::new();
    if is_error {
        output.push(json!({"type": "input_text", "text": TOOL_RESULT_ERROR_MARKER}));
    }
    match content {
        Value::String(text) => output.push(json!({"type": "input_text", "text": text})),
        Value::Array(parts) => {
            for part in parts {
                let part = part.as_object().ok_or(BridgeRequestError::InvalidRequest)?;
                if part.get("type").and_then(Value::as_str) != Some("text") {
                    return Err(BridgeRequestError::InvalidRequest);
                }
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(BridgeRequestError::InvalidRequest)?;
                output.push(json!({"type": "input_text", "text": text}));
            }
        }
        _ => return Err(BridgeRequestError::InvalidRequest),
    }
    Ok(Value::Array(output))
}

fn convert_tools(tools: Option<&Value>) -> Result<Vec<Value>, BridgeRequestError> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };
    let tools = tools.as_array().ok_or(BridgeRequestError::InvalidRequest)?;
    tools
        .iter()
        .map(|tool| {
            let tool = tool.as_object().ok_or(BridgeRequestError::InvalidRequest)?;
            let name = required_nonempty_string(tool, "name")?;
            let description = match tool.get("description") {
                None | Some(Value::Null) => Value::Null,
                Some(Value::String(description)) => Value::String(description.clone()),
                Some(_) => return Err(BridgeRequestError::InvalidRequest),
            };
            let parameters = tool
                .get("input_schema")
                .filter(|schema| schema.is_object())
                .cloned()
                .map(|schema| clean_tool_schema(schema, true))
                .ok_or(BridgeRequestError::InvalidRequest)?;
            Ok(json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters
            }))
        })
        .collect()
}

fn clean_tool_schema(mut schema: Value, root: bool) -> Value {
    let Some(object) = schema.as_object_mut() else {
        return schema;
    };
    let missing_type = root && !object.contains_key("type");
    if missing_type {
        object.insert("type".to_owned(), Value::String("object".to_owned()));
        object
            .entry("properties".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if object.get("format").and_then(Value::as_str) == Some("uri") {
        object.remove("format");
    }
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for value in properties.values_mut() {
            *value = clean_tool_schema(value.clone(), false);
        }
    }
    if let Some(items) = object.get_mut("items") {
        *items = clean_tool_schema(items.clone(), false);
    }
    schema
}

fn convert_tool_choice(choice: &Value) -> Result<Value, BridgeRequestError> {
    let kind = match choice {
        Value::String(kind) => kind.as_str(),
        Value::Object(choice) => choice
            .get("type")
            .and_then(Value::as_str)
            .ok_or(BridgeRequestError::InvalidRequest)?,
        _ => return Err(BridgeRequestError::InvalidRequest),
    };
    match kind {
        "any" => Ok(Value::String("required".to_owned())),
        "auto" | "none" => Ok(Value::String(kind.to_owned())),
        "tool" => {
            let choice = choice
                .as_object()
                .ok_or(BridgeRequestError::InvalidRequest)?;
            let name = required_nonempty_string(choice, "name")?;
            Ok(json!({"type": "function", "name": name}))
        }
        _ => Err(BridgeRequestError::InvalidRequest),
    }
}

fn canonical_json_string(value: &Value) -> Result<String, BridgeRequestError> {
    serde_json::to_string(&canonicalize(value)).map_err(|_| BridgeRequestError::InvalidRequest)
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        value => value.clone(),
    }
}

fn session_identity(
    source: &Map<String, Value>,
    inbound_headers: &HeaderMap,
) -> Result<Option<String>, BridgeRequestError> {
    let metadata = match source.get("metadata") {
        None => None,
        Some(Value::Object(metadata)) => Some(metadata),
        Some(_) => return Err(BridgeRequestError::InvalidRequest),
    };
    let user_id = metadata
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let suffix = user_id.and_then(|value| {
        value
            .split_once("_session_")
            .map(|(_, suffix)| suffix)
            .filter(|suffix| !suffix.is_empty())
    });
    let metadata_session = metadata
        .and_then(|metadata| metadata.get("session_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let header_session = inbound_headers
        .get("x-session-id")
        .map(|value| {
            value
                .to_str()
                .map_err(|_| BridgeRequestError::InvalidRequest)
        })
        .transpose()?
        .filter(|value| !value.is_empty());
    Ok(suffix
        .or(metadata_session)
        .or(user_id)
        .or(header_session)
        .map(str::to_owned))
}

fn identity_headers(
    account_id: &str,
    access_token: &str,
    session: Option<&str>,
) -> Result<HeaderMap, BridgeRequestError> {
    if account_id.is_empty() || access_token.is_empty() {
        return Err(BridgeRequestError::InvalidRequest);
    }
    let mut headers = HeaderMap::new();
    insert_header(
        &mut headers,
        header::AUTHORIZATION,
        &format!("Bearer {access_token}"),
    )?;
    insert_static_header(&mut headers, "chatgpt-account-id", account_id)?;
    insert_static_header(&mut headers, "originator", ORIGINATOR)?;
    insert_static_header(&mut headers, "version", CODEX_VERSION)?;
    insert_header(&mut headers, header::CONTENT_TYPE, "application/json")?;
    if let Some(session) = session {
        insert_static_header(&mut headers, "session_id", session)?;
        insert_static_header(&mut headers, "x-client-request-id", session)?;
        insert_static_header(&mut headers, "x-codex-window-id", &format!("{session}:0"))?;
    }
    Ok(headers)
}

fn insert_static_header(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), BridgeRequestError> {
    insert_header(headers, HeaderName::from_static(name), value)
}

fn insert_header(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &str,
) -> Result<(), BridgeRequestError> {
    let value = HeaderValue::from_str(value).map_err(|_| BridgeRequestError::InvalidRequest)?;
    headers.insert(name, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::body::Bytes;
    use axum::http::{HeaderMap, HeaderValue};
    use futures_util::{StreamExt, stream};
    use secrecy::SecretString;
    use serde_json::Value;

    use super::{
        BridgeRequestError, BridgeRequestInput, SUBSCRIPTION_BRIDGE_RESPONSES_URL,
        SubscriptionBridgeAdapter,
    };

    const ACCOUNT_ID: &str = "FIXTURE_ACCOUNT_ID_8127";
    const ACCESS_TOKEN: &str = "FIXTURE_ACCESS_TOKEN_9471";
    const REQUEST_CONTENT: &str = "FIXTURE_REQUEST_CONTENT_6521";

    fn fixture(name: &str) -> Value {
        serde_json::from_str(fixture_source(name)).expect("fixture must be valid JSON")
    }

    fn fixture_source(name: &str) -> &'static str {
        match name {
            "messages-text.input.json" => {
                include_str!("../../tests/fixtures/subscription-bridge/messages-text.input.json")
            }
            "messages-text.expected.json" => {
                include_str!("../../tests/fixtures/subscription-bridge/messages-text.expected.json")
            }
            "messages-tools.input.json" => {
                include_str!("../../tests/fixtures/subscription-bridge/messages-tools.input.json")
            }
            "messages-tools.expected.json" => {
                include_str!(
                    "../../tests/fixtures/subscription-bridge/messages-tools.expected.json"
                )
            }
            "responses-stream.input.sse" => {
                include_str!("../../tests/fixtures/subscription-bridge/responses-stream.input.sse")
            }
            "anthropic-stream.expected.sse" => {
                include_str!(
                    "../../tests/fixtures/subscription-bridge/anthropic-stream.expected.sse"
                )
            }
            _ => panic!("unknown subscription bridge fixture source"),
        }
    }

    fn prepare(
        model: &str,
        body: Value,
        headers: HeaderMap,
    ) -> Result<super::PreparedBridgeRequest, BridgeRequestError> {
        SubscriptionBridgeAdapter::prepare(BridgeRequestInput {
            path: "/v1/messages",
            provider_model: model,
            account_id: ACCOUNT_ID,
            access_token: &SecretString::from(ACCESS_TOKEN),
            inbound_headers: &headers,
            body: &serde_json::to_vec(&body).expect("fixture serialization must succeed"),
        })
    }

    fn assert_no_secret_surface<T: std::fmt::Debug>(value: &T) {
        let debug = format!("{value:?}");
        assert!(
            !debug.contains(ACCOUNT_ID)
                && !debug.contains(ACCESS_TOKEN)
                && !debug.contains(REQUEST_CONTENT),
            "subscription bridge diagnostic exposed fixture identity"
        );
    }

    fn assert_value_eq_redacted(actual: &Value, expected: &Value, label: &'static str) {
        assert!(
            actual == expected,
            "subscription bridge fixture mismatch: {label}"
        );
    }

    fn assert_header_eq_redacted(
        prepared: &super::PreparedBridgeRequest,
        name: &str,
        expected: Option<&str>,
        label: &'static str,
    ) {
        assert!(
            prepared.header(name) == expected,
            "subscription bridge header mismatch: {label}"
        );
    }

    fn parse_sse(contents: &str) -> Vec<(String, Value)> {
        contents
            .replace("\r\n", "\n")
            .split("\n\n")
            .filter_map(|block| {
                let mut event = None;
                let mut data = Vec::new();
                for line in block.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        event = Some(value.trim().to_owned());
                    } else if let Some(value) = line.strip_prefix("data:") {
                        data.push(value.trim_start());
                    }
                }
                if data.is_empty() {
                    return None;
                }
                let data = serde_json::from_str(&data.join("\n"))
                    .expect("fixture SSE data must be valid JSON");
                Some((event.expect("fixture SSE event must be named"), data))
            })
            .collect()
    }

    #[test]
    fn fixture_manifest_pins_oracle_provenance_and_content_hashes() {
        let manifest: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/subscription-bridge/manifest.json"
        ))
        .expect("fixture manifest must be valid JSON");
        assert!(
            manifest.pointer("/oracle/tag").and_then(Value::as_str) == Some("v3.19.2")
                && manifest.pointer("/oracle/commit").and_then(Value::as_str)
                    == Some("43eaf07355af145aebfee301801779e824d4c221"),
            "subscription bridge oracle provenance mismatch"
        );
        let entries = manifest
            .get("fixtures")
            .and_then(Value::as_array)
            .expect("fixture manifest must contain fixtures");
        assert!(
            entries.len() == 6,
            "subscription bridge fixture count mismatch"
        );
        for entry in entries {
            let name = entry
                .get("file")
                .and_then(Value::as_str)
                .expect("fixture entry must name a file");
            let expected = entry
                .get("sha256")
                .and_then(Value::as_str)
                .expect("fixture entry must contain a hash");
            let actual =
                ring::digest::digest(&ring::digest::SHA256, fixture_source(name).as_bytes())
                    .as_ref()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
            assert!(
                actual == expected,
                "subscription bridge fixture hash mismatch"
            );
        }
    }

    #[test]
    fn encodes_text_fixture_and_fixed_identity_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-session-id",
            HeaderValue::from_static("inbound-session-ignored"),
        );
        let prepared = prepare("gpt-5.6", fixture("messages-text.input.json"), headers)
            .expect("text bridge request must encode");
        assert_no_secret_surface(&prepared);

        assert!(
            prepared.url() == SUBSCRIPTION_BRIDGE_RESPONSES_URL,
            "subscription bridge endpoint mismatch"
        );
        assert_value_eq_redacted(
            prepared.body(),
            &fixture("messages-text.expected.json"),
            "text",
        );
        assert_header_eq_redacted(
            &prepared,
            "authorization",
            Some("Bearer FIXTURE_ACCESS_TOKEN_9471"),
            "authorization",
        );
        assert_header_eq_redacted(&prepared, "chatgpt-account-id", Some(ACCOUNT_ID), "account");
        assert_header_eq_redacted(&prepared, "originator", Some("codex_cli_rs"), "originator");
        assert_header_eq_redacted(&prepared, "version", Some("0.144.1"), "version");
        assert_header_eq_redacted(
            &prepared,
            "content-type",
            Some("application/json"),
            "content-type",
        );
        assert_header_eq_redacted(&prepared, "session_id", Some("metadata-session"), "session");
        assert_header_eq_redacted(
            &prepared,
            "x-client-request-id",
            Some("metadata-session"),
            "request-id",
        );
        assert_header_eq_redacted(
            &prepared,
            "x-codex-window-id",
            Some("metadata-session:0"),
            "window-id",
        );
    }

    #[test]
    fn encodes_tools_and_canonical_function_arguments() {
        let prepared = prepare(
            "gpt-5.6-luna",
            fixture("messages-tools.input.json"),
            HeaderMap::new(),
        )
        .expect("tools bridge request must encode");
        assert_no_secret_surface(&prepared);
        assert_value_eq_redacted(
            prepared.body(),
            &fixture("messages-tools.expected.json"),
            "tools",
        );
        assert_header_eq_redacted(
            &prepared,
            "session_id",
            Some("session-from-user"),
            "tool-session",
        );
    }

    #[test]
    fn applies_closed_session_precedence_without_inventing_a_session() {
        let cases = [
            (
                serde_json::json!({
                    "model": "ignored",
                    "metadata": {"user_id": "raw_session_suffix", "session_id": "metadata-session"},
                    "messages": []
                }),
                Some("suffix"),
            ),
            (
                serde_json::json!({
                    "model": "ignored",
                    "metadata": {"user_id": "raw-user", "session_id": "metadata-session"},
                    "messages": []
                }),
                Some("metadata-session"),
            ),
            (
                serde_json::json!({
                    "model": "ignored",
                    "metadata": {"user_id": "raw-user"},
                    "messages": []
                }),
                Some("raw-user"),
            ),
            (
                serde_json::json!({"model": "ignored", "messages": []}),
                Some("header-session"),
            ),
            (
                serde_json::json!({"model": "ignored", "messages": []}),
                None,
            ),
        ];

        for (body, expected) in cases {
            let mut headers = HeaderMap::new();
            if expected == Some("header-session") {
                headers.insert("x-session-id", HeaderValue::from_static("header-session"));
            }
            let prepared = prepare("gpt-5.6", body, headers).expect("case must encode");
            assert_header_eq_redacted(&prepared, "session_id", expected, "session precedence");
            assert_header_eq_redacted(
                &prepared,
                "x-client-request-id",
                expected,
                "client request id precedence",
            );
            assert_header_eq_redacted(
                &prepared,
                "x-codex-window-id",
                expected.map(|session| format!("{session}:0")).as_deref(),
                "window id precedence",
            );
        }
    }

    #[test]
    fn redacted_fixture_mismatch_diagnostic_never_contains_request_content() {
        let actual = serde_json::json!({"secret": REQUEST_CONTENT});
        let expected = serde_json::json!({"secret": "different"});
        let panic = std::panic::catch_unwind(|| {
            assert_value_eq_redacted(&actual, &expected, "controlled-mutation")
        })
        .expect_err("controlled mismatch must fail");
        let diagnostic = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unexpected panic payload");
        assert!(
            !diagnostic.contains(REQUEST_CONTENT),
            "redacted fixture comparison exposed controlled request content"
        );
    }

    #[tokio::test]
    async fn converts_chunk_split_text_tools_usage_and_completion_fixture() {
        let input = fixture_source("responses-stream.input.sse").as_bytes();
        let multibyte = input
            .windows("世".len())
            .position(|window| window == "世".as_bytes())
            .expect("stream fixture must contain multibyte text");
        let mut split_points = vec![1, 7, 31, 103, 211, multibyte + 1, 557, input.len()];
        split_points.sort_unstable();
        split_points.dedup();
        let mut start = 0;
        let chunks = split_points.into_iter().map(move |end| {
            let end = end.min(input.len());
            let chunk = Bytes::copy_from_slice(&input[start..end]);
            start = end;
            Ok::<_, std::io::Error>(chunk)
        });
        let converted = SubscriptionBridgeAdapter::convert_stream(stream::iter(chunks));
        let actual = converted
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|chunk| String::from_utf8(chunk.to_vec()).expect("bridge SSE must be UTF-8"))
            .collect::<String>();
        assert!(
            parse_sse(&actual) == parse_sse(fixture_source("anthropic-stream.expected.sse")),
            "subscription bridge streaming fixture mismatch"
        );
    }

    #[tokio::test]
    async fn converts_reasoning_and_incomplete_stop_reason() {
        let upstream = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_reason\",\"model\":\"gpt-5.6-luna\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"reason_1\",\"type\":\"reasoning\"}}\n\n",
            "event: response.reasoning_summary_text.delta\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"reason_1\",\"delta\":\"Checking.\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"reason_1\",\"type\":\"reasoning\"}}\n\n",
            "event: response.incomplete\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}}\n\n"
        );
        let actual =
            SubscriptionBridgeAdapter::convert_stream(stream::iter([Ok::<_, std::io::Error>(
                Bytes::from_static(upstream.as_bytes()),
            )]))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|chunk| String::from_utf8(chunk.to_vec()).expect("bridge SSE must be UTF-8"))
            .collect::<String>();
        let events = parse_sse(&actual);
        assert!(
            events.iter().any(
                |(_, event)| event.pointer("/delta/type").and_then(Value::as_str)
                    == Some("thinking_delta")
            ) && events.iter().any(|(_, event)| event
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
                == Some("max_tokens")),
            "subscription bridge reasoning/incomplete mapping mismatch"
        );
    }

    #[tokio::test]
    async fn rejects_failed_malformed_contradictory_oversized_and_incomplete_streams() {
        let response_secret = "FIXTURE_RESPONSE_SECRET_7319";
        let cases = [
            format!(
                "event: response.failed\ndata: {{\"type\":\"response.failed\",\"response\":{{\"error\":{{\"message\":\"{response_secret}\"}}}}}}\n\n"
            )
            .into_bytes(),
            format!(
                "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"message\":\"{response_secret}\"}}}}\n\n"
            )
            .into_bytes(),
            b"event: response.created\ndata: {not-json}\n\n".to_vec(),
            b"event: response.created\ndata: {\"type\":\"response.completed\"}\n\n".to_vec(),
            vec![b'a'; super::MAX_BRIDGE_SSE_EVENT_BYTES + 1],
            b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"unfinished\",\"model\":\"gpt-5.6\"}}\n\n".to_vec(),
        ];
        for bytes in cases {
            let actual =
                SubscriptionBridgeAdapter::convert_stream(stream::iter([Ok::<_, std::io::Error>(
                    Bytes::from(bytes),
                )]))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|chunk| String::from_utf8(chunk.to_vec()).expect("bridge SSE must be UTF-8"))
                .collect::<String>();
            assert!(
                !actual.contains(response_secret)
                    && parse_sse(&actual).last().is_some_and(|(name, event)| {
                        name == "error"
                            && event
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .is_some_and(|message| {
                                    matches!(
                                        message,
                                        "subscription-bridge-upstream-error"
                                            | "subscription-bridge-invalid-response"
                                    )
                                })
                    }),
                "subscription bridge invalid stream diagnostic mismatch"
            );
        }
    }

    #[test]
    fn maps_non_success_status_with_a_bounded_fixed_error() {
        let response_secret = "FIXTURE_HTTP_RESPONSE_SECRET_2184";
        let mapped = SubscriptionBridgeAdapter::map_non_success(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            format!("{{\"error\":\"{response_secret}\"}}").as_bytes(),
        )
        .expect("bounded non-success body must map");
        assert_no_secret_surface(&mapped);
        let debug = format!("{mapped:?}");
        let event = String::from_utf8(mapped.event().to_vec()).expect("error SSE must be UTF-8");
        assert!(
            mapped.status() == axum::http::StatusCode::TOO_MANY_REQUESTS
                && !debug.contains(response_secret)
                && !event.contains(response_secret)
                && event.contains("subscription-bridge-upstream-error"),
            "subscription bridge non-success mapping mismatch"
        );
        let oversized = vec![b'x'; super::MAX_BRIDGE_ERROR_BODY_BYTES + 1];
        assert!(matches!(
            SubscriptionBridgeAdapter::map_non_success(
                axum::http::StatusCode::BAD_GATEWAY,
                &oversized
            ),
            Err(super::BridgeResponseError::InvalidResponse)
        ));
    }

    struct DropObservedStream {
        yielded: bool,
        drops: Arc<AtomicUsize>,
    }

    impl futures_util::Stream for DropObservedStream {
        type Item = Result<Bytes, std::io::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if self.yielded {
                std::task::Poll::Pending
            } else {
                self.yielded = true;
                std::task::Poll::Ready(Some(Ok(Bytes::from_static(
                    b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"cancel\",\"model\":\"gpt-5.6\"}}\n\n",
                ))))
            }
        }
    }

    impl Drop for DropObservedStream {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn dropping_downstream_stream_drops_upstream_without_a_detached_task() {
        let drops = Arc::new(AtomicUsize::new(0));
        let upstream = DropObservedStream {
            yielded: false,
            drops: Arc::clone(&drops),
        };
        let mut converted = Box::pin(SubscriptionBridgeAdapter::convert_stream(upstream));
        let first = converted
            .as_mut()
            .next()
            .await
            .expect("created must produce a frame");
        assert!(
            String::from_utf8_lossy(&first).contains("message_start"),
            "subscription bridge cancellation fixture did not start"
        );
        drop(converted);
        assert!(
            drops.load(Ordering::SeqCst) == 1,
            "dropping downstream did not synchronously drop upstream ownership"
        );
    }

    #[test]
    fn rejects_invalid_unsupported_and_oversized_requests_locally() {
        let invalid = [
            b"[]".as_slice(),
            br#"{"messages":"not-an-array"}"#,
            br#"{"messages":[{"role":"user","content":[{"type":"image","source":{}}]}]}"#,
            br#"{"messages":[{"role":"system","content":"wrong place"}]}"#,
            br#"{"messages":[{"role":"user","content":[{"type":"tool_use","name":"missing-id"}]}]}"#,
        ];
        for body in invalid {
            let result = SubscriptionBridgeAdapter::prepare(BridgeRequestInput {
                path: "/v1/messages",
                provider_model: "gpt-5.6",
                account_id: ACCOUNT_ID,
                access_token: &SecretString::from(ACCESS_TOKEN),
                inbound_headers: &HeaderMap::new(),
                body,
            });
            assert_no_secret_surface(&result);
            assert!(
                matches!(result, Err(BridgeRequestError::InvalidRequest)),
                "invalid bridge input returned an unexpected classification"
            );
        }

        let oversized = vec![b' '; super::MAX_BRIDGE_BODY_BYTES + 1];
        let result = SubscriptionBridgeAdapter::prepare(BridgeRequestInput {
            path: "/v1/messages",
            provider_model: "gpt-5.6",
            account_id: ACCOUNT_ID,
            access_token: &SecretString::from(ACCESS_TOKEN),
            inbound_headers: &HeaderMap::new(),
            body: &oversized,
        });
        assert!(matches!(result, Err(BridgeRequestError::RequestTooLarge)));

        let result = SubscriptionBridgeAdapter::prepare(BridgeRequestInput {
            path: "/v1/messages/count_tokens",
            provider_model: "gpt-5.6",
            account_id: ACCOUNT_ID,
            access_token: &SecretString::from(ACCESS_TOKEN),
            inbound_headers: &HeaderMap::new(),
            body: br#"{"messages":[]}"#,
        });
        assert!(matches!(
            result,
            Err(BridgeRequestError::CountTokensUnsupported)
        ));

        let prepared = prepare(
            "unclaimed-model",
            serde_json::json!({"messages": [], "include": ["untrusted-field"]}),
            HeaderMap::new(),
        )
        .expect("unclaimed provider model must pass through without a compatibility claim");
        assert!(
            prepared.body().get("model") == Some(&Value::String("unclaimed-model".to_owned()))
                && prepared.body().get("include")
                    == Some(&serde_json::json!(["reasoning.encrypted_content"])),
            "subscription bridge model/include ownership mismatch"
        );
    }
}
