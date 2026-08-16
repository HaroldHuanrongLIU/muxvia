#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use muxvia_routing::{
    claude::CommandClaudeProbe,
    codex::CommandCodexProbe,
    control::{
        framing::{read_frame, write_frame},
        protocol::Target,
        server::ControlServer,
    },
    home::MuxviaHome,
    model::{UpstreamError, UpstreamRequest, UpstreamResponse, UpstreamTransport},
    service::activate::ActivationService,
    state::StateStore,
};
use serde_json::{Value, json};
use tokio::{
    io::AsyncReadExt,
    net::{TcpStream, UnixStream},
};
use uuid::Uuid;

struct NoopUpstream;

#[async_trait]
impl UpstreamTransport for NoopUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        Err(UpstreamError)
    }
}

async fn hello(stream: &mut UnixStream) {
    write_frame(
        stream,
        &json!({
            "type": "hello",
            "rpc": { "major": 1, "minor": 0 },
            "release": "reconciliation-test"
        }),
    )
    .await
    .unwrap();
    assert_eq!(read_frame(stream).await.unwrap()["type"], "hello-ack");
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

async fn request_raw(stream: &mut UnixStream, request_id: &str, operation: Value) -> Vec<u8> {
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
    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix).await.unwrap();
    let mut body = vec![0_u8; u32::from_be_bytes(prefix) as usize];
    stream.read_exact(&mut body).await.unwrap();
    body
}

fn assert_raw_frame_is_safe(raw: &[u8], sentinels: &[&str]) {
    for sentinel in sentinels {
        assert!(
            !raw_frame_exposes(raw, sentinel),
            "raw UDS frame exposed literal or numeric sentinel bytes"
        );
    }
}

fn raw_frame_exposes(raw: &[u8], sentinel: &str) -> bool {
    let text = std::str::from_utf8(raw).unwrap();
    let numeric = sentinel
        .as_bytes()
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    text.contains(sentinel) || text.contains(&numeric)
}

#[test]
fn raw_frame_scan_rejects_embedded_compact_numeric_sentinel_bytes() {
    let sentinel = "RAW_NUMERIC_SCAN_SENTINEL_95701";
    let mut bytes = vec![1_u8];
    bytes.extend_from_slice(sentinel.as_bytes());
    bytes.push(2);
    let raw = serde_json::to_vec(&json!({ "bytes": bytes })).unwrap();
    assert!(
        raw_frame_exposes(&raw, sentinel),
        "scan accepted a sentinel embedded as compact JSON numeric bytes"
    );
}

async fn assert_held_connection_is_alive(connection: &TcpStream) {
    let mut byte = [0_u8; 1];
    match tokio::time::timeout(Duration::from_millis(20), connection.peek(&mut byte)).await {
        Err(_) | Ok(Ok(1..)) => {}
        Ok(Ok(0)) => panic!("preview closed a held runtime connection"),
        Ok(Err(_)) => panic!("preview invalidated a held runtime connection"),
    }
}

struct ProviderFixture<'a> {
    name: &'a str,
    base_url: &'a str,
    model: &'a str,
    authentication: &'a str,
    secret: &'a str,
}

fn open_operation(target: Target, user_home: &Path) -> Value {
    match target {
        Target::Codex => json!({ "kind": "open-target", "target": "codex" }),
        Target::Claude => json!({
            "kind": "open-target",
            "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "unset",
                "hostManagedState": "unmanaged",
                "cwd": user_home
            }
        }),
    }
}

async fn activate_takeover(
    socket: &Path,
    target: Target,
    user_home: &Path,
    provider: ProviderFixture<'_>,
) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    hello(&mut stream).await;
    let opened = request(&mut stream, "open", open_operation(target, user_home)).await;
    let revision = opened["result"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let saved = request(
        &mut stream,
        "save",
        json!({
            "kind": "act",
            "target": target.as_str(),
            "actionId": Uuid::new_v4(),
            "expectedRevision": revision,
            "action": {
                "kind": "create-provider",
                "name": provider.name,
                "baseUrl": provider.base_url,
                "model": provider.model,
                "credential": { "kind": "replace", "value": provider.secret },
                "authentication": provider.authentication,
                "presetKey": null
            }
        }),
    )
    .await;
    let provider_id = saved["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .unwrap();
    let revision = saved["result"]["outcome"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let _push = read_frame(&mut stream).await.unwrap();
    let activated = request(
        &mut stream,
        "activate",
        json!({
            "kind": "act",
            "target": target.as_str(),
            "actionId": Uuid::new_v4(),
            "expectedRevision": revision,
            "action": {
                "kind": "activate-provider",
                "providerId": provider_id,
                "mode": "takeover"
            }
        }),
    )
    .await;
    assert_eq!(activated["type"], "response");
    let _push = read_frame(&mut stream).await.unwrap();
}

fn probe_executable(root: &Path, target: Target) -> PathBuf {
    let executable = root.join(format!("{}-probe", target.as_str()));
    let script = match target {
        Target::Codex => {
            "#!/bin/sh\ncase \"$1\" in\n --version) printf 'codex-cli 77.1.0\\n'; printf 'CODEX_VERSION_STDERR_SENTINEL_95101\\n' >&2 ;;\n --help) printf 'Usage: codex [OPTIONS]\\n--config <key=value>\\nCODEX_HELP_STDOUT_SENTINEL_95102\\n'; printf 'CODEX_HELP_STDERR_SENTINEL_95103\\n' >&2 ;;\n *) exit 91 ;;\nesac\n"
        }
        Target::Claude => {
            "#!/bin/sh\ncase \"$1\" in\n --version) printf '77.1.0 (Claude Code)\\n'; printf 'CLAUDE_VERSION_STDERR_SENTINEL_95201\\n' >&2 ;;\n --help) printf 'Usage: claude [options]\\n--settings <file>\\n--model <model>\\nCLAUDE_HELP_STDOUT_SENTINEL_95202\\n'; printf 'CLAUDE_HELP_STDERR_SENTINEL_95203\\n' >&2 ;;\n *) exit 91 ;;\nesac\n"
        }
    };
    fs::write(&executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

#[tokio::test]
async fn preview_all_targets_and_strategies_preserves_every_durable_surface_and_publishes_nothing()
{
    let root = std::path::PathBuf::from("/tmp").join(format!(
        "mx-preview-six-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let codex_executable = probe_executable(&root, Target::Codex);
    let claude_executable = probe_executable(&root, Target::Claude);
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(CommandCodexProbe),
            codex_executable,
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), claude_executable),
    );
    let handle = ControlServer::bind_with_activation(
        &home,
        Arc::clone(&store),
        "test",
        Arc::clone(&activation),
    )
    .await
    .unwrap();
    activate_takeover(
        handle.socket_path(),
        Target::Codex,
        &user_home,
        ProviderFixture {
            name: "Codex",
            base_url: "https://BASE_URL_RAW_SENTINEL_95301.test/v1",
            model: "MODEL_RAW_SENTINEL_95302",
            authentication: "openai-bearer",
            secret: "CREDENTIAL_RAW_SENTINEL_95303",
        },
    )
    .await;
    activate_takeover(
        handle.socket_path(),
        Target::Claude,
        &user_home,
        ProviderFixture {
            name: "Claude",
            base_url: "https://BASE_URL_RAW_SENTINEL_95401.test",
            model: "MODEL_RAW_SENTINEL_95402",
            authentication: "anthropic-api-key",
            secret: "CREDENTIAL_RAW_SENTINEL_95403",
        },
    )
    .await;

    let codex_endpoint = activation.model_endpoint_for(Target::Codex).await.unwrap();
    let claude_endpoint = activation.model_endpoint_for(Target::Claude).await.unwrap();
    let codex_connection = TcpStream::connect(codex_endpoint).await.unwrap();
    let claude_connection = TcpStream::connect(claude_endpoint).await.unwrap();
    let codex_connection_identity = (
        codex_connection.local_addr().unwrap(),
        codex_connection.peer_addr().unwrap(),
    );
    let claude_connection_identity = (
        claude_connection.local_addr().unwrap(),
        claude_connection.peer_addr().unwrap(),
    );

    let mut codex_document = fs::read_to_string(user_home.join(".codex/config.toml")).unwrap();
    codex_document
        .push_str("\nunrelated_preview_value = \"UNRELATED_CONFIG_RAW_SENTINEL_95501\"\n");
    fs::write(user_home.join(".codex/config.toml"), codex_document).unwrap();
    let mut claude_document: serde_json::Value =
        serde_json::from_slice(&fs::read(user_home.join(".claude/settings.json")).unwrap())
            .unwrap();
    claude_document["unrelatedPreviewValue"] = json!("UNRELATED_CONFIG_RAW_SENTINEL_95502");
    fs::write(
        user_home.join(".claude/settings.json"),
        serde_json::to_vec(&claude_document).unwrap(),
    )
    .unwrap();

    let before_database = fs::read(home.database_path()).unwrap();
    let before_codex = fs::read(user_home.join(".codex/config.toml")).unwrap();
    let before_claude = fs::read(user_home.join(".claude/settings.json")).unwrap();
    let before_codex_view = store.target_view_for(Target::Codex).await.unwrap();
    let before_claude_view = store.target_view_for(Target::Claude).await.unwrap();
    let mut published = store.subscribe_target_views();

    let mut first_reapply_token = None;
    for target in [Target::Codex, Target::Claude] {
        let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut stream).await;
        let _opened = request(&mut stream, "open", open_operation(target, &user_home)).await;
        for strategy in ["adopt", "reapply", "restore"] {
            let raw = request_raw(
                &mut stream,
                strategy,
                json!({
                    "kind": "preview-reconciliation",
                    "target": target.as_str(),
                    "strategy": strategy
                }),
            )
            .await;
            assert_raw_frame_is_safe(
                &raw,
                &[
                    "CREDENTIAL_RAW_SENTINEL_95303",
                    "CREDENTIAL_RAW_SENTINEL_95403",
                    "MODEL_RAW_SENTINEL_95302",
                    "MODEL_RAW_SENTINEL_95402",
                    "BASE_URL_RAW_SENTINEL_95301",
                    "BASE_URL_RAW_SENTINEL_95401",
                    "UNRELATED_CONFIG_RAW_SENTINEL_95501",
                    "UNRELATED_CONFIG_RAW_SENTINEL_95502",
                    "CODEX_VERSION_STDERR_SENTINEL_95101",
                    "CODEX_HELP_STDOUT_SENTINEL_95102",
                    "CODEX_HELP_STDERR_SENTINEL_95103",
                    "CLAUDE_VERSION_STDERR_SENTINEL_95201",
                    "CLAUDE_HELP_STDOUT_SENTINEL_95202",
                    "CLAUDE_HELP_STDERR_SENTINEL_95203",
                ],
            );
            let response: Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(response["type"], "response");
            assert_eq!(response["result"]["kind"], "reconciliation-preview");
            assert_eq!(response["result"]["preview"]["target"], target.as_str());
            assert_eq!(response["result"]["preview"]["strategy"], strategy);
            if target == Target::Codex && strategy == "reapply" {
                first_reapply_token = response["result"]["preview"]["observationToken"]
                    .as_str()
                    .map(str::to_owned);
            }
            assert_eq!(
                activation.model_endpoint_for(Target::Codex).await,
                Some(codex_endpoint)
            );
            assert_eq!(
                activation.model_endpoint_for(Target::Claude).await,
                Some(claude_endpoint)
            );
            assert_eq!(
                (
                    codex_connection.local_addr().unwrap(),
                    codex_connection.peer_addr().unwrap()
                ),
                codex_connection_identity
            );
            assert_eq!(
                (
                    claude_connection.local_addr().unwrap(),
                    claude_connection.peer_addr().unwrap()
                ),
                claude_connection_identity
            );
            assert_held_connection_is_alive(&codex_connection).await;
            assert_held_connection_is_alive(&claude_connection).await;
            assert!(TcpStream::connect(codex_endpoint).await.is_ok());
            assert!(TcpStream::connect(claude_endpoint).await.is_ok());
            assert_eq!(fs::read(home.database_path()).unwrap(), before_database);
            assert_eq!(
                fs::read(user_home.join(".codex/config.toml")).unwrap(),
                before_codex
            );
            assert_eq!(
                fs::read(user_home.join(".claude/settings.json")).unwrap(),
                before_claude
            );
            assert_eq!(
                store.target_view_for(Target::Codex).await.unwrap(),
                before_codex_view
            );
            assert_eq!(
                store.target_view_for(Target::Claude).await.unwrap(),
                before_claude_view
            );
            assert!(
                matches!(
                    published.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ),
                "preview published a TargetView subscription update"
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), read_frame(&mut stream))
                .await
                .is_err(),
            "preview emitted a TargetView push"
        );
    }

    assert_eq!(handle.tracked_reconciliation_tokens().await, 6);
    let mut codex = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut codex).await;
    let _opened = request(
        &mut codex,
        "open-again",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    let replacement = request(
        &mut codex,
        "replacement",
        json!({
            "kind": "preview-reconciliation",
            "target": "codex",
            "strategy": "reapply"
        }),
    )
    .await;
    assert_ne!(
        replacement["result"]["preview"]["observationToken"],
        first_reapply_token.unwrap()
    );
    assert_eq!(handle.tracked_reconciliation_tokens().await, 6);

    assert_eq!(fs::read(home.database_path()).unwrap(), before_database);
    assert_eq!(
        fs::read(user_home.join(".codex/config.toml")).unwrap(),
        before_codex
    );
    assert_eq!(
        fs::read(user_home.join(".claude/settings.json")).unwrap(),
        before_claude
    );
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        before_codex_view
    );
    assert_eq!(
        store.target_view_for(Target::Claude).await.unwrap(),
        before_claude_view
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), published.recv())
            .await
            .is_err(),
        "preview published a TargetView subscription update"
    );

    fs::write(
        user_home.join(".codex/config.toml"),
        "invalid = [\"RAW_FAILURE_CONFIG_SENTINEL_95601\"\n",
    )
    .unwrap();
    let raw_failure = request_raw(
        &mut codex,
        "fixed-failure",
        json!({
            "kind": "preview-reconciliation",
            "target": "codex",
            "strategy": "reapply"
        }),
    )
    .await;
    assert_raw_frame_is_safe(
        &raw_failure,
        &[
            "RAW_FAILURE_CONFIG_SENTINEL_95601",
            "CODEX_VERSION_STDERR_SENTINEL_95101",
            "CODEX_HELP_STDOUT_SENTINEL_95102",
            "CODEX_HELP_STDERR_SENTINEL_95103",
        ],
    );
    let failure: Value = serde_json::from_slice(&raw_failure).unwrap();
    assert_eq!(failure["type"], "error");
    assert_eq!(
        failure["problem"],
        json!({
            "code": "configuration-write-failed",
            "message": "Reconciliation preview failed"
        })
    );

    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();
    let _ = fs::remove_dir_all(root);
}
