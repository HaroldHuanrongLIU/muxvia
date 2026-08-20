use std::{collections::BTreeMap, fmt};

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value, json};

pub(crate) const SUBSCRIPTION_BRIDGE_RESPONSES_URL: &str =
    "https://chatgpt.com/backend-api/codex/responses";
pub(crate) const MAX_BRIDGE_BODY_BYTES: usize = 32 * 1024 * 1024;

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
    use axum::http::{HeaderMap, HeaderValue};
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
            entries.len() == 4,
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
