#![cfg(unix)]

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use axum::{
    body::Bytes,
    http::{HeaderMap, HeaderValue, StatusCode, header},
};
use base64::Engine as _;
use futures_util::stream;
use muxvia_routing::{
    claude::{ClaudeCapability, ClaudeProbe, ClaudeProblem, CommandClaudeProbe},
    codex::{CodexCapability, CodexProbe, CodexProblem, CommandCodexProbe},
    control::{
        framing::{read_frame, write_frame},
        protocol::{
            ClaudeHostManagedState, ClaudePreflightContext, ClaudeSelectorState, RouteHealthState,
            Target,
        },
        server::ControlServer,
    },
    home::MuxviaHome,
    model::{UpstreamError, UpstreamRequest, UpstreamResponse, UpstreamTransport},
    service::activate::ActivationService,
    state::StateStore,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixStream},
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const FIXED_ACCESS: &str = "BRIDGE_FIXED_ACCESS_SECRET_12901";
const PRIMARY_ACCESS: &str = "BRIDGE_PRIMARY_ACCESS_SECRET_12902";
const SECONDARY_ACCESS: &str = "BRIDGE_SECONDARY_ACCESS_SECRET_12903";
const PINNED_ACCESS: &str = "BRIDGE_PINNED_ACCESS_SECRET_12907";
const ROUTING_SECRET_MARKER: &str = "ANTHROPIC_AUTH_TOKEN";

struct TestedCodexProbe;

impl CodexProbe for TestedCodexProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        Ok(CodexCapability::Tested {
            version: "bridge-test".to_owned(),
        })
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                _ = cancellation.cancelled() => CommandCodexProbe.probe(Path::new("relative")),
                result = async { self.probe(Path::new("/usr/bin/codex")) } => result,
            }
        })
    }
}

struct TestedClaudeProbe;

impl ClaudeProbe for TestedClaudeProbe {
    fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
        Ok(ClaudeCapability::Tested {
            version: "bridge-test".to_owned(),
        })
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ClaudeCapability, ClaudeProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                _ = cancellation.cancelled() => CommandClaudeProbe.probe(Path::new("relative")),
                result = async { self.probe(Path::new("/usr/bin/claude")) } => result,
            }
        })
    }
}

struct CapturedBridgeRequest {
    url: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

struct BridgeUpstream {
    requests: Mutex<Vec<CapturedBridgeRequest>>,
    bridge_statuses: Mutex<VecDeque<StatusCode>>,
    native_statuses: Mutex<VecDeque<StatusCode>>,
    incomplete_bridge_response: AtomicBool,
    failed_bridge_response: AtomicBool,
}

#[tokio::test]
async fn bridge_listener_without_installed_account_runtime_fails_closed() {
    let root = short_temp_root("mx-bridge-no-runtime");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let created = store
        .apply_provider_action_for(
            Target::Claude,
            Uuid::new_v4(),
            0,
            json!({
                "kind": "create-provider",
                "name": "Bridge without runtime",
                "baseUrl": "https://chatgpt.com/backend-api/codex",
                "model": "gpt-5.6",
                "credential": {"kind": "remove"},
                "authentication": "codex-subscription",
                "presetKey": "codex-subscription-bridge"
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;
    install_binding(&home, provider_id, "fixed", Some("account-fixed")).await;
    let upstream = Arc::new(BridgeUpstream {
        requests: Mutex::new(Vec::new()),
        bridge_statuses: Mutex::new(VecDeque::new()),
        native_statuses: Mutex::new(VecDeque::new()),
        incomplete_bridge_response: AtomicBool::new(false),
        failed_bridge_response: AtomicBool::new(false),
    });
    let activation = ActivationService::new(
        store,
        home.clone(),
        Arc::new(TestedCodexProbe),
        PathBuf::from("/usr/bin/codex"),
        upstream.clone(),
    )
    .with_claude_runtime(
        Arc::new(TestedClaudeProbe),
        PathBuf::from("/usr/bin/claude"),
    );
    let context = ClaudePreflightContext {
        claude_config_dir: None,
        selector_state: ClaudeSelectorState::Unset,
        blocking_selector: None,
        host_managed_state: ClaudeHostManagedState::Unmanaged,
        cwd: user_home.to_string_lossy().into_owned(),
    };
    let applied = activation
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            created.view.management_revision,
            json!({"kind": "activate-provider", "providerId": provider_id, "mode": "takeover"}),
            Some(&context),
        )
        .await
        .expect("activate Bridge without control runtime");
    let endpoint = activation
        .model_endpoint_for(Target::Claude)
        .await
        .expect("Bridge listener");
    let settings: Value = serde_json::from_slice(
        &fs::read(user_home.join(".claude/settings.json")).expect("Claude settings"),
    )
    .unwrap();
    let routing_token = settings["env"]["ANTHROPIC_AUTH_TOKEN"]
        .as_str()
        .expect("routing credential");
    let response = send_message(&reqwest::Client::new(), endpoint, routing_token).await;
    assert!(
        applied.view.mode == "takeover"
            && response.status() == StatusCode::BAD_GATEWAY
            && response.text().await.unwrap() == "subscription-account-unavailable"
            && upstream.requests.lock().unwrap().is_empty(),
        "Bridge listener without an installed resolver did not fail closed before upstream"
    );
    activation.shutdown_models().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[async_trait]
impl UpstreamTransport for BridgeUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let is_bridge = request.url.as_str() == "https://chatgpt.com/backend-api/codex/responses";
        self.requests.lock().unwrap().push(CapturedBridgeRequest {
            url: request.url.to_string(),
            headers: request.headers,
            body: request.body.as_bytes().unwrap_or_default().to_vec(),
        });
        let mut headers = HeaderMap::new();
        if is_bridge {
            let status = self
                .bridge_statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(StatusCode::OK);
            if !status.is_success() {
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                return Ok(UpstreamResponse {
                    status,
                    headers,
                    body: Box::pin(stream::once(async {
                        Ok(Bytes::from_static(
                            b"BRIDGE_RAW_UPSTREAM_ERROR_SECRET_12908",
                        ))
                    })),
                });
            }
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            let body = if self.failed_bridge_response.swap(false, Ordering::SeqCst) {
                Bytes::from_static(include_bytes!(
                    "fixtures/subscription-bridge/responses-error.input.sse"
                ))
            } else if self
                .incomplete_bridge_response
                .swap(false, Ordering::SeqCst)
            {
                Bytes::from_static(include_bytes!(
                    "fixtures/subscription-bridge/responses-incomplete.input.sse"
                ))
            } else {
                Bytes::from_static(include_bytes!(
                    "fixtures/subscription-bridge/responses-stream.input.sse"
                ))
            };
            Ok(UpstreamResponse {
                status: StatusCode::OK,
                headers,
                body: Box::pin(stream::once(async move { Ok(body) })),
            })
        } else {
            let status = self
                .native_statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(StatusCode::OK);
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            Ok(UpstreamResponse {
                status,
                headers,
                body: Box::pin(stream::once(async {
                    Ok(Bytes::from_static(
                        br#"{"type":"message","content":[],"stop_reason":"end_turn"}"#,
                    ))
                })),
            })
        }
    }
}

#[tokio::test]
async fn bridge_resolves_fixed_and_current_default_then_fails_over_without_substitution() {
    let root = short_temp_root("mx-bridge-route");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    install_accounts(&home);
    let authority = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority_origin = format!("http://{}", authority.local_addr().unwrap());
    let refreshes = Arc::new(Mutex::new(Vec::<String>::new()));
    let authority_refreshes = refreshes.clone();
    let (pinned_started_tx, mut pinned_started_rx) = mpsc::unbounded_channel();
    let (release_pinned_tx, release_pinned_rx) = oneshot::channel();
    let authority_task = tokio::spawn(async move {
        let mut release_pinned_rx = Some(release_pinned_rx);
        loop {
            let Ok((mut socket, _)) = authority.accept().await else {
                return;
            };
            let request = read_http_request(&mut socket).await;
            if request.contains("refresh-pinned") {
                let _ = pinned_started_tx.send(());
                if let Some(release) = release_pinned_rx.take() {
                    let _ = release.await;
                }
            }
            let response = refresh_response(&request);
            authority_refreshes
                .lock()
                .unwrap()
                .push(refresh_kind(&request));
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.0,
                        response.1.len(),
                        response.1
                    )
                    .as_bytes(),
                )
                .await;
        }
    });

    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let bridge = store
        .apply_provider_action_for(
            Target::Claude,
            Uuid::new_v4(),
            0,
            json!({
                "kind": "create-provider",
                "name": "Codex Subscription Bridge",
                "baseUrl": "https://chatgpt.com/backend-api/codex",
                "model": "gpt-5.6-luna",
                "credential": {"kind": "remove"},
                "authentication": "codex-subscription",
                "presetKey": "codex-subscription-bridge"
            }),
        )
        .await
        .unwrap();
    let bridge_id = bridge.view.providers[0].id;
    let native = store
        .apply_provider_action_for(
            Target::Claude,
            Uuid::new_v4(),
            bridge.view.management_revision,
            json!({
                "kind": "create-provider",
                "name": "Native fallback",
                "baseUrl": "https://native.example/v1",
                "model": "claude-fallback",
                "credential": {"kind": "replace", "value": "NATIVE_PROVIDER_SECRET_12904"},
                "authentication": "anthropic-bearer",
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    let native_provider = native
        .view
        .providers
        .iter()
        .find(|provider| provider.name == "Native fallback")
        .unwrap();
    let native_id = native_provider.id;
    install_binding(&home, bridge_id, "fixed", Some("account-fixed")).await;

    let upstream = Arc::new(BridgeUpstream {
        requests: Mutex::new(Vec::new()),
        bridge_statuses: Mutex::new(VecDeque::new()),
        native_statuses: Mutex::new(VecDeque::new()),
        incomplete_bridge_response: AtomicBool::new(false),
        failed_bridge_response: AtomicBool::new(false),
    });
    let activation = Arc::new(
        ActivationService::new(
            store.clone(),
            home.clone(),
            Arc::new(TestedCodexProbe),
            PathBuf::from("/usr/bin/codex"),
            upstream.clone(),
        )
        .with_claude_runtime(
            Arc::new(TestedClaudeProbe),
            PathBuf::from("/usr/bin/claude"),
        ),
    );
    let handle = ControlServer::bind_with_activation_and_device_authority_origin(
        &home,
        store.clone(),
        "bridge-test",
        activation.clone(),
        &authority_origin,
    )
    .await
    .unwrap();
    let mut control = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut control).await;
    let opened = request(
        &mut control,
        "open",
        json!({
            "kind": "open-target", "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "unset",
                "hostManagedState": "unmanaged",
                "cwd": user_home
            }
        }),
    )
    .await;
    assert_secret_free_frame(&opened);
    let activated = request(
        &mut control,
        "activate",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": opened["result"]["view"]["managementRevision"],
            "action": {"kind": "activate-provider", "providerId": bridge_id, "mode": "takeover"}
        }),
    )
    .await;
    assert_secret_free_frame(&activated);
    let activation_push = read_frame(&mut control).await.unwrap();
    assert_secret_free_frame(&activation_push);
    let settings: Value = serde_json::from_slice(
        &fs::read(user_home.join(".claude/settings.json")).expect("Claude settings"),
    )
    .unwrap();
    let routing_token = settings["env"]["ANTHROPIC_AUTH_TOKEN"]
        .as_str()
        .expect("routing credential")
        .to_owned();
    for frame in [&opened, &activated, &activation_push] {
        assert_frame_excludes_routing_credential(frame, &routing_token);
    }
    assert!(activated["type"] == "response", "Bridge activation failed");
    let saved = request(
        &mut control,
        "save-plan",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": activated["result"]["outcome"]["view"]["managementRevision"],
            "action": {
                "kind": "save-failover-draft",
                "members": [
                    {"providerId": bridge_id, "providerRevision": 1},
                    {"providerId": native_id, "providerRevision": native_provider.provider_revision}
                ]
            }
        }),
    )
    .await;
    assert_secret_free_frame(&saved);
    assert_frame_excludes_routing_credential(&saved, &routing_token);
    let save_push = read_frame(&mut control).await.unwrap();
    assert_secret_free_frame(&save_push);
    assert_frame_excludes_routing_credential(&save_push, &routing_token);
    let applied = request(
        &mut control,
        "apply-plan",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": saved["result"]["outcome"]["view"]["managementRevision"],
            "action": {
                "kind": "apply-failover-chain",
                "draftRevision": saved["result"]["outcome"]["view"]["failover"]["draftRevision"]
            }
        }),
    )
    .await;
    assert_secret_free_frame(&applied);
    assert_frame_excludes_routing_credential(&applied, &routing_token);
    assert!(applied["type"] == "response", "Bridge failover plan failed");
    let apply_push = read_frame(&mut control).await.unwrap();
    assert_secret_free_frame(&apply_push);
    assert_frame_excludes_routing_credential(&apply_push, &routing_token);

    drop(control);
    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();
    let restarted_activation = Arc::new(
        ActivationService::new(
            store.clone(),
            home.clone(),
            Arc::new(TestedCodexProbe),
            PathBuf::from("/usr/bin/codex"),
            upstream.clone(),
        )
        .with_claude_runtime(
            Arc::new(TestedClaudeProbe),
            PathBuf::from("/usr/bin/claude"),
        ),
    );
    let restarted = ControlServer::bind_with_activation_and_device_authority_origin(
        &home,
        store.clone(),
        "bridge-restart-test",
        restarted_activation.clone(),
        &authority_origin,
    )
    .await
    .unwrap();
    let endpoint = restarted_activation
        .model_endpoint_for(Target::Claude)
        .await
        .expect("committed Bridge listener resumed with its account resolver");
    let client = reqwest::Client::new();

    let fixed = send_message(&client, endpoint, &routing_token).await;
    assert!(
        fixed.status() == StatusCode::OK,
        "Fixed Bridge request did not succeed"
    );
    let fixed_body = fixed.bytes().await.unwrap();
    assert_bridge_request(&upstream, 0, "account-fixed", FIXED_ACCESS);
    assert!(
        sse_data_values(&fixed_body)
            == sse_data_values(include_bytes!(
                "fixtures/subscription-bridge/anthropic-stream.expected.sse"
            )),
        "Fixed Bridge response conversion changed"
    );

    install_binding(&home, bridge_id, "follow-default", None).await;
    let primary = send_message(&client, endpoint, &routing_token).await;
    assert!(
        primary.status() == StatusCode::OK,
        "default Bridge request failed"
    );
    let _ = primary.bytes().await.unwrap();
    assert_bridge_request(&upstream, 1, "account-primary", PRIMARY_ACCESS);

    change_default(&home, Some("account-secondary"));
    let secondary = send_message(&client, endpoint, &routing_token).await;
    assert!(
        secondary.status() == StatusCode::OK,
        "changed default was not used"
    );
    let _ = secondary.bytes().await.unwrap();
    assert_bridge_request(&upstream, 2, "account-secondary", SECONDARY_ACCESS);

    install_binding(&home, bridge_id, "fixed", Some("account-pinned")).await;
    let pinned_index = upstream.requests.lock().unwrap().len();
    let pinned_client = client.clone();
    let pinned_routing_token = routing_token.clone();
    let pinned_request =
        tokio::spawn(
            async move { send_message(&pinned_client, endpoint, &pinned_routing_token).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(1), pinned_started_rx.recv())
        .await
        .expect("pinned account refresh did not start")
        .expect("pinned refresh channel closed");
    install_binding(&home, bridge_id, "follow-default", None).await;
    change_default(&home, Some("account-secondary"));
    let _ = release_pinned_tx.send(());
    let pinned_response = pinned_request.await.unwrap();
    assert!(
        pinned_response.status() == StatusCode::OK,
        "pinned Bridge request did not complete"
    );
    let _ = pinned_response.bytes().await.unwrap();
    assert_bridge_request(&upstream, pinned_index, "account-pinned", PINNED_ACCESS);

    upstream
        .bridge_statuses
        .lock()
        .unwrap()
        .push_back(StatusCode::SERVICE_UNAVAILABLE);
    let before_retryable = upstream.requests.lock().unwrap().len();
    let retryable = send_message(&client, endpoint, &routing_token).await;
    assert!(
        retryable.status() == StatusCode::OK,
        "retryable Bridge upstream status did not advance to the next Provider"
    );
    let retryable_body = retryable.bytes().await.unwrap();
    assert!(
        !retryable_body
            .windows("BRIDGE_RAW_UPSTREAM_ERROR_SECRET_12908".len())
            .any(|window| window == b"BRIDGE_RAW_UPSTREAM_ERROR_SECRET_12908")
            && upstream.requests.lock().unwrap().len() == before_retryable + 2,
        "retryable Bridge upstream body escaped or fallback was not attempted"
    );
    let after_retryable = store.target_view_for(Target::Claude).await.unwrap();
    assert!(
        after_retryable.serving_provider_id.as_deref() == Some(&native_id.to_string()),
        "retryable Bridge failure did not record the serving fallback Provider"
    );
    install_binding(&home, bridge_id, "fixed", Some("account-fixed")).await;
    send_and_assert_bridge_success(&client, endpoint, &routing_token, &upstream).await;

    upstream
        .incomplete_bridge_response
        .store(true, Ordering::SeqCst);
    let before_incomplete = upstream.requests.lock().unwrap().len();
    let incomplete = send_message(&client, endpoint, &routing_token).await;
    let incomplete_status = incomplete.status();
    let incomplete_content_type = incomplete
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let incomplete_body = incomplete.bytes().await.unwrap();
    assert!(
        incomplete_status == StatusCode::OK
            && incomplete_content_type.as_deref() == Some("text/event-stream")
            && sse_data_values(&incomplete_body).iter().any(|event| {
                event.pointer("/delta/stop_reason").and_then(Value::as_str) == Some("max_tokens")
            })
            && upstream.requests.lock().unwrap().len() == before_incomplete + 1,
        "pre-output incomplete Bridge response bypassed its Anthropic conversion"
    );

    upstream
        .failed_bridge_response
        .store(true, Ordering::SeqCst);
    let before_failed = upstream.requests.lock().unwrap().len();
    let failed = send_message(&client, endpoint, &routing_token).await;
    let failed_status = failed.status();
    let failed_content_type = failed
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let failed_body = failed.bytes().await.unwrap();
    assert!(
        failed_status == StatusCode::OK
            && failed_content_type.as_deref() == Some("text/event-stream")
            && sse_data_values(&failed_body).iter().any(|event| {
                event.pointer("/error/message").and_then(Value::as_str)
                    == Some("subscription-bridge-upstream-error")
            })
            && !failed_body
                .windows("FIXTURE_RESPONSE_SECRET_7319".len())
                .any(|window| window == b"FIXTURE_RESPONSE_SECRET_7319")
            && upstream.requests.lock().unwrap().len() == before_failed + 1,
        "pre-output failed Bridge response bypassed its fixed Anthropic conversion"
    );
    let after_recovery = store.target_view_for(Target::Claude).await.unwrap();
    assert!(
        after_recovery.serving_provider_id.as_deref() == Some(&bridge_id.to_string())
            && after_recovery.providers.iter().any(|provider| {
                provider.id == bridge_id
                    && provider.route_health.state == RouteHealthState::Degraded
            }),
        "converted Bridge failure was not retained as a real Route Health failure"
    );

    install_binding(&home, bridge_id, "fixed", Some("missing-account")).await;
    send_and_assert_native_fallback(&client, endpoint, &routing_token, &upstream).await;
    install_binding(&home, bridge_id, "fixed", Some("account-fixed")).await;
    send_and_assert_bridge_success(&client, endpoint, &routing_token, &upstream).await;

    install_binding(&home, bridge_id, "follow-default", None).await;
    change_default(&home, None);
    send_and_assert_native_fallback(&client, endpoint, &routing_token, &upstream).await;
    install_binding(&home, bridge_id, "fixed", Some("account-fixed")).await;
    send_and_assert_bridge_success(&client, endpoint, &routing_token, &upstream).await;

    install_binding(&home, bridge_id, "fixed", Some("account-needs")).await;
    send_and_assert_native_fallback(&client, endpoint, &routing_token, &upstream).await;
    install_binding(&home, bridge_id, "fixed", Some("account-fixed")).await;
    send_and_assert_bridge_success(&client, endpoint, &routing_token, &upstream).await;

    install_binding(&home, bridge_id, "fixed", Some("account-transient")).await;
    send_and_assert_native_fallback(&client, endpoint, &routing_token, &upstream).await;
    install_binding(&home, bridge_id, "fixed", Some("account-fixed")).await;
    send_and_assert_bridge_success(&client, endpoint, &routing_token, &upstream).await;

    install_binding(&home, bridge_id, "fixed", Some("account-permanent")).await;
    send_and_assert_native_fallback(&client, endpoint, &routing_token, &upstream).await;
    let after_permanent: Value = serde_json::from_slice(
        &fs::read(home.subscription_accounts_path()).expect("account file after rejection"),
    )
    .unwrap();
    assert!(
        after_permanent["accounts"]["account-permanent"]["state"] == "needs-reauthorization",
        "permanent refresh rejection did not persist Needs Reauthorization"
    );
    let refresh_count_after_rejection = refreshes.lock().unwrap().len();
    send_and_assert_native_fallback(&client, endpoint, &routing_token, &upstream).await;
    assert!(
        refreshes.lock().unwrap().len() == refresh_count_after_rejection,
        "persistent Needs Reauthorization retried the rejected refresh token"
    );

    install_binding(&home, bridge_id, "fixed", Some("account-mismatch")).await;
    send_and_assert_native_fallback(&client, endpoint, &routing_token, &upstream).await;

    let before_invalid_upstream = upstream.requests.lock().unwrap().len();
    let before_invalid_refresh = refreshes.lock().unwrap().len();
    let invalid = client
        .post(format!("http://{endpoint}/v1/messages"))
        .header(header::AUTHORIZATION, format!("Bearer {routing_token}"))
        .body("{")
        .send()
        .await
        .unwrap();
    assert!(
        invalid.status() == StatusCode::BAD_REQUEST
            && invalid.text().await.unwrap() == "subscription-bridge-invalid-request"
            && upstream.requests.lock().unwrap().len() == before_invalid_upstream
            && refreshes.lock().unwrap().len() == before_invalid_refresh,
        "malformed Bridge input did not fail locally with its fixed category"
    );

    let before_count = upstream.requests.lock().unwrap().len();
    install_binding(&home, bridge_id, "follow-default", None).await;
    let count = client
        .post(format!("http://{endpoint}/v1/messages/count_tokens"))
        .header(header::AUTHORIZATION, format!("Bearer {routing_token}"))
        .body(include_bytes!("fixtures/subscription-bridge/messages-text.input.json").as_slice())
        .send()
        .await
        .unwrap();
    assert!(
        count.status() == StatusCode::NOT_IMPLEMENTED
            && count.text().await.unwrap() == "subscription-bridge-count-tokens-unsupported"
            && upstream.requests.lock().unwrap().len() == before_count,
        "Bridge count_tokens contacted an account/upstream or returned the wrong deviation"
    );

    let mut replanning = UnixStream::connect(restarted.socket_path()).await.unwrap();
    hello(&mut replanning).await;
    let reopened = request(
        &mut replanning,
        "reopen-for-count-tokens",
        json!({
            "kind": "open-target", "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "unset",
                "hostManagedState": "unmanaged",
                "cwd": user_home
            }
        }),
    )
    .await;
    let native_current = request(
        &mut replanning,
        "activate-native-for-count-tokens",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": reopened["result"]["view"]["managementRevision"],
            "action": {
                "kind": "activate-provider",
                "providerId": native_id,
                "mode": "takeover"
            }
        }),
    )
    .await;
    assert!(
        native_current["type"] == "response",
        "native Provider did not become Current before the reversed route"
    );
    let _native_current_push = read_frame(&mut replanning).await.unwrap();
    let reversed = request(
        &mut replanning,
        "reverse-plan",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": native_current["result"]["outcome"]["view"]["managementRevision"],
            "action": {
                "kind": "save-failover-draft",
                "members": [
                    {"providerId": native_id, "providerRevision": native_provider.provider_revision},
                    {"providerId": bridge_id, "providerRevision": 1}
                ]
            }
        }),
    )
    .await;
    assert!(
        reversed["type"] == "response",
        "native-first failover draft did not save"
    );
    let _reverse_push = read_frame(&mut replanning).await.unwrap();
    let reversed_applied = request(
        &mut replanning,
        "apply-reversed-plan",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": reversed["result"]["outcome"]["view"]["managementRevision"],
            "action": {
                "kind": "apply-failover-chain",
                "draftRevision": reversed["result"]["outcome"]["view"]["failover"]["draftRevision"]
            }
        }),
    )
    .await;
    assert!(
        reversed_applied["type"] == "response",
        "native-first Failover Chain did not apply"
    );
    let _reversed_push = read_frame(&mut replanning).await.unwrap();
    drop(replanning);
    install_binding(&home, bridge_id, "fixed", Some("account-transient")).await;
    upstream
        .native_statuses
        .lock()
        .unwrap()
        .push_back(StatusCode::SERVICE_UNAVAILABLE);
    let before_fallback_count = upstream.requests.lock().unwrap().len();
    let before_fallback_refresh = refreshes.lock().unwrap().len();
    let fallback_count = client
        .post(format!("http://{endpoint}/v1/messages/count_tokens"))
        .header(header::AUTHORIZATION, format!("Bearer {routing_token}"))
        .body(include_bytes!("fixtures/subscription-bridge/messages-text.input.json").as_slice())
        .send()
        .await
        .unwrap();
    assert!(
        fallback_count.status() == StatusCode::NOT_IMPLEMENTED
            && fallback_count.text().await.unwrap()
                == "subscription-bridge-count-tokens-unsupported"
            && upstream.requests.lock().unwrap().len() == before_fallback_count + 1
            && refreshes.lock().unwrap().len() == before_fallback_refresh,
        "fallback Bridge count_tokens contacted its account/upstream or lost the named deviation"
    );

    let refresh_kinds = refreshes.lock().unwrap().clone();
    assert!(
        refresh_kinds
            == [
                "fixed",
                "primary",
                "secondary",
                "pinned",
                "transient",
                "permanent",
                "mismatch",
            ],
        "binding resolution refreshed a substituted or unexpected account"
    );
    restarted.shutdown().await.unwrap();
    authority_task.abort();
    let _ = fs::remove_dir_all(root);
}

async fn send_message(
    client: &reqwest::Client,
    endpoint: std::net::SocketAddr,
    routing_token: &str,
) -> reqwest::Response {
    client
        .post(format!("http://{endpoint}/v1/messages"))
        .header(header::AUTHORIZATION, format!("Bearer {routing_token}"))
        .header("x-session-id", "BRIDGE_SESSION_SECRET_12905")
        .body(include_bytes!("fixtures/subscription-bridge/messages-tools.input.json").as_slice())
        .send()
        .await
        .unwrap()
}

async fn send_and_assert_native_fallback(
    client: &reqwest::Client,
    endpoint: std::net::SocketAddr,
    routing_token: &str,
    upstream: &BridgeUpstream,
) {
    let before = upstream.requests.lock().unwrap().len();
    let response = send_message(client, endpoint, routing_token).await;
    assert!(
        response.status() == StatusCode::OK,
        "unavailable Bridge member did not advance to the next Provider"
    );
    let _body = response.bytes().await.unwrap();
    let requests = upstream.requests.lock().unwrap();
    assert!(
        requests.len() == before + 1
            && requests
                .last()
                .is_some_and(|request| { request.url == "https://native.example/v1/messages" }),
        "unavailable Bridge member substituted an account or contacted its upstream"
    );
}

async fn send_and_assert_bridge_success(
    client: &reqwest::Client,
    endpoint: std::net::SocketAddr,
    routing_token: &str,
    upstream: &BridgeUpstream,
) {
    let before = upstream.requests.lock().unwrap().len();
    let response = send_message(client, endpoint, routing_token).await;
    assert!(
        response.status() == StatusCode::OK,
        "available Bridge member did not recover route health"
    );
    let _body = response.bytes().await.unwrap();
    let requests = upstream.requests.lock().unwrap();
    assert!(
        requests.len() == before + 1
            && requests.last().is_some_and(|request| {
                request.url == "https://chatgpt.com/backend-api/codex/responses"
            }),
        "available Bridge member did not own the recovered attempt"
    );
}

fn assert_bridge_request(
    upstream: &BridgeUpstream,
    index: usize,
    account_id: &str,
    access_token: &str,
) {
    let requests = upstream.requests.lock().unwrap();
    let Some(request) = requests.get(index) else {
        panic!("Bridge request was not captured");
    };
    let body = serde_json::from_slice::<Value>(&request.body).ok();
    let expected_body = serde_json::from_slice::<Value>(include_bytes!(
        "fixtures/subscription-bridge/messages-tools.expected.json"
    ))
    .ok();
    let exact = request.url == "https://chatgpt.com/backend-api/codex/responses"
        && request
            .headers
            .get("chatgpt-account-id")
            .and_then(|value| value.to_str().ok())
            == Some(account_id)
        && request
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            == Some(&format!("Bearer {access_token}"))
        && body
            .as_ref()
            .and_then(|value| value.get("model"))
            .and_then(Value::as_str)
            == Some("gpt-5.6-luna")
        && body == expected_body;
    assert!(
        exact,
        "Bridge endpoint, identity headers, or converted body changed"
    );
}

fn assert_secret_free_frame(frame: &Value) {
    let rendered = frame.to_string();
    for forbidden in [
        FIXED_ACCESS,
        PRIMARY_ACCESS,
        SECONDARY_ACCESS,
        PINNED_ACCESS,
        "MISMATCH_ACCESS_SECRET_12906",
        "BRIDGE_RAW_UPSTREAM_ERROR_SECRET_12908",
        "BRIDGE_SESSION_SECRET_12905",
        "NATIVE_PROVIDER_SECRET_12904",
        ROUTING_SECRET_MARKER,
    ] {
        assert!(
            !rendered.contains(forbidden),
            "Bridge control frame exposed private routing or account material"
        );
    }
}

fn assert_frame_excludes_routing_credential(frame: &Value, routing_token: &str) {
    assert!(
        !frame.to_string().contains(routing_token),
        "Bridge control frame exposed the generated routing credential"
    );
}

fn sse_data_values(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|value| serde_json::from_str(value).ok())
        .collect()
}

fn install_accounts(home: &MuxviaHome) {
    let accounts = [
        ("account-fixed", "refresh-fixed", "authorized"),
        ("account-primary", "refresh-primary", "authorized"),
        ("account-secondary", "refresh-secondary", "authorized"),
        ("account-pinned", "refresh-pinned", "authorized"),
        ("account-needs", "refresh-needs", "needs-reauthorization"),
        ("account-transient", "refresh-transient", "authorized"),
        ("account-permanent", "refresh-permanent", "authorized"),
        ("account-mismatch", "refresh-mismatch", "authorized"),
    ]
    .into_iter()
    .map(|(account_id, refresh_token, state)| {
        (
            account_id.to_owned(),
            json!({
                "account_id": account_id,
                "email": null,
                "refresh_token": refresh_token,
                "authenticated_at": 1,
                "state": state
            }),
        )
    })
    .collect::<BTreeMap<_, _>>();
    fs::create_dir_all(home.subscription_accounts_path().parent().unwrap()).unwrap();
    fs::write(
        home.subscription_accounts_path(),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "accounts": accounts,
            "default_account_id": "account-primary"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(
        home.subscription_accounts_path(),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}

fn change_default(home: &MuxviaHome, account_id: Option<&str>) {
    let mut document: Value =
        serde_json::from_slice(&fs::read(home.subscription_accounts_path()).unwrap()).unwrap();
    document["default_account_id"] = account_id.map_or(Value::Null, |value| json!(value));
    fs::write(
        home.subscription_accounts_path(),
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
}

async fn install_binding(
    home: &MuxviaHome,
    provider_id: Uuid,
    kind: &str,
    account_id: Option<&str>,
) {
    let database = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap();
    let kind = kind.to_owned();
    let account_id = account_id.map(str::to_owned);
    database
        .call(move |connection| {
            connection.execute(
                "INSERT INTO subscription_provider_bindings
                    (target, provider_id, binding_kind, account_id)
                 VALUES ('claude', ?1, ?2, ?3)
                 ON CONFLICT(target, provider_id) DO UPDATE SET
                    binding_kind = excluded.binding_kind,
                    account_id = excluded.account_id",
                tokio_rusqlite::rusqlite::params![provider_id.to_string(), kind, account_id],
            )?;
            Ok::<(), tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
}

fn refresh_kind(request: &str) -> String {
    for (needle, kind) in [
        ("refresh-fixed", "fixed"),
        ("refresh-primary", "primary"),
        ("refresh-secondary", "secondary"),
        ("refresh-pinned", "pinned"),
        ("refresh-transient", "transient"),
        ("refresh-permanent", "permanent"),
        ("refresh-mismatch", "mismatch"),
    ] {
        if request.contains(needle) {
            return kind.to_owned();
        }
    }
    "unknown".to_owned()
}

fn refresh_response(request: &str) -> (&'static str, String) {
    let kind = refresh_kind(request);
    match kind.as_str() {
        "fixed" => token_response(FIXED_ACCESS, None),
        "primary" => token_response(PRIMARY_ACCESS, None),
        "secondary" => token_response(SECONDARY_ACCESS, None),
        "pinned" => token_response(PINNED_ACCESS, None),
        "transient" => ("500 Internal Server Error", String::new()),
        "permanent" => ("401 Unauthorized", String::new()),
        "mismatch" => {
            let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({"chatgpt_account_id": "different-account"})).unwrap(),
            );
            token_response(
                "MISMATCH_ACCESS_SECRET_12906",
                Some(format!("e30.{claims}.sig")),
            )
        }
        _ => ("400 Bad Request", String::new()),
    }
}

fn token_response(access_token: &str, id_token: Option<String>) -> (&'static str, String) {
    (
        "200 OK",
        json!({
            "access_token": access_token,
            "refresh_token": null,
            "id_token": id_token,
            "expires_in": 3600
        })
        .to_string(),
    )
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = socket.read(&mut buffer).await.unwrap();
        assert!(read != 0, "refresh request ended early");
        bytes.extend_from_slice(&buffer[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + length {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

async fn hello(stream: &mut UnixStream) {
    write_frame(
        stream,
        &json!({
            "type": "hello",
            "rpc": {"major": 1, "minor": 0},
            "release": "subscription-bridge-test"
        }),
    )
    .await
    .unwrap();
    let _hello = read_frame(stream).await.unwrap();
}

async fn request(stream: &mut UnixStream, request_id: &str, operation: Value) -> Value {
    write_frame(
        stream,
        &json!({
            "type": "request",
            "requestId": request_id,
            "operation": operation
        }),
    )
    .await
    .unwrap();
    read_frame(stream).await.unwrap()
}

fn short_temp_root(prefix: &str) -> PathBuf {
    let id = Uuid::new_v4().simple().to_string();
    PathBuf::from("/tmp").join(format!("{prefix}-{}", &id[..8]))
}
