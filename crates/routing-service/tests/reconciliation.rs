#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures_util::stream;
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
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio::{
    io::AsyncReadExt,
    net::{TcpStream, UnixStream},
    sync::{Mutex, Notify},
};
use uuid::Uuid;

struct NoopUpstream;

#[async_trait]
impl UpstreamTransport for NoopUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        Err(UpstreamError)
    }
}

struct HeldUpstream {
    started: Notify,
    release: Notify,
    urls: Mutex<Vec<String>>,
}

impl HeldUpstream {
    fn new() -> Self {
        Self {
            started: Notify::new(),
            release: Notify::new(),
            urls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl UpstreamTransport for HeldUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        self.urls.lock().await.push(request.url.to_string());
        self.started.notify_one();
        self.release.notified().await;
        Ok(UpstreamResponse {
            status: axum::http::StatusCode::OK,
            headers: axum::http::HeaderMap::new(),
            body: Box::pin(stream::once(async {
                Ok(axum::body::Bytes::from_static(b"{}"))
            })),
        })
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

fn rewrite_probe_as_incompatible(executable: &Path, target: Target) {
    let script = match target {
        Target::Codex => {
            "#!/bin/sh\ncase \"$1\" in\n --version) printf 'codex-cli 77.1.0\\n'; printf 'CODEX_VERSION_STDERR_SENTINEL_95101\\n' >&2 ;;\n --help) printf 'Usage: codex [OPTIONS]\\nCODEX_HELP_STDOUT_SENTINEL_95102\\n'; printf 'CODEX_HELP_STDERR_SENTINEL_95103\\n' >&2 ;;\n *) exit 91 ;;\nesac\n"
        }
        Target::Claude => {
            "#!/bin/sh\ncase \"$1\" in\n --version) printf '77.1.0 (Claude Code)\\n'; printf 'CLAUDE_VERSION_STDERR_SENTINEL_95201\\n' >&2 ;;\n --help) printf 'Usage: claude [options]\\nCLAUDE_HELP_STDOUT_SENTINEL_95202\\n'; printf 'CLAUDE_HELP_STDERR_SENTINEL_95203\\n' >&2 ;;\n *) exit 91 ;;\nesac\n"
        }
    };
    fs::write(executable, script).unwrap();
    fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
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
            codex_executable.clone(),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), claude_executable.clone()),
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
    rewrite_probe_as_incompatible(&codex_executable, Target::Codex);
    rewrite_probe_as_incompatible(&claude_executable, Target::Claude);

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
            assert_eq!(
                response["result"]["preview"]["compatibility"],
                match target {
                    Target::Codex => json!({
                        "version": "codex-cli 77.1.0",
                        "classification": "incompatible",
                        "acknowledgementRequired": false
                    }),
                    Target::Claude => json!({
                        "version": "77.1.0 (Claude Code)",
                        "classification": "incompatible",
                        "acknowledgementRequired": false
                    }),
                }
            );
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

#[tokio::test]
async fn reapply_reconciliation_over_real_uds_restores_owned_and_preserves_unrelated_configuration()
{
    let root = std::path::PathBuf::from("/tmp").join(format!(
        "mx-reapply-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(CommandCodexProbe),
            probe_executable(&root, Target::Codex),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(
            Arc::new(CommandClaudeProbe),
            probe_executable(&root, Target::Claude),
        ),
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
            name: "Committed Codex",
            base_url: "https://committed.example/v1",
            model: "committed-model",
            authentication: "openai-bearer",
            secret: "COMMITTED_PROVIDER_SECRET_96101",
        },
    )
    .await;
    let committed = fs::read_to_string(user_home.join(".codex/config.toml")).unwrap();
    let drifted = committed.replace("committed-model", "external-model")
        + "\nunrelated_after_preview = \"preserve-me\"\n";
    fs::write(user_home.join(".codex/config.toml"), drifted).unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    let revision = opened["result"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let preview = request(
        &mut stream,
        "preview",
        json!({
            "kind": "preview-reconciliation",
            "target": "codex",
            "strategy": "reapply"
        }),
    )
    .await;
    assert_eq!(preview["result"]["kind"], "reconciliation-preview");
    let token = preview["result"]["preview"]["observationToken"]
        .as_str()
        .unwrap();
    let outcome = request(
        &mut stream,
        "apply",
        json!({
            "kind": "act",
            "target": "codex",
            "actionId": Uuid::new_v4(),
            "expectedRevision": revision,
            "action": {
                "kind": "reconcile",
                "strategy": "reapply",
                "observationToken": token,
                "acknowledgeVersion": "codex-cli 77.1.0"
            }
        }),
    )
    .await;
    assert_eq!(outcome["type"], "response");
    assert_eq!(outcome["result"]["outcome"]["status"], "applied");
    assert_eq!(
        outcome["result"]["outcome"]["view"]["managementRevision"],
        revision + 1
    );
    let rendered = fs::read_to_string(user_home.join(".codex/config.toml")).unwrap();
    assert!(rendered.contains("committed-model"));
    assert!(!rendered.contains("external-model"));
    assert!(rendered.contains("unrelated_after_preview = \"preserve-me\""));
    let push = read_frame(&mut stream).await.unwrap();
    assert_eq!(push["type"], "target-view");

    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn adopt_reconciliation_creates_immutable_history_and_receipt_first_replay() {
    let root = std::path::PathBuf::from("/tmp").join(format!(
        "mx-adopt-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(CommandCodexProbe),
            probe_executable(&root, Target::Codex),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(
            Arc::new(CommandClaudeProbe),
            probe_executable(&root, Target::Claude),
        ),
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
            name: "Historical provider",
            base_url: "https://historical.example/v1",
            model: "historical-model",
            authentication: "openai-bearer",
            secret: "HISTORICAL_SECRET_96201",
        },
    )
    .await;
    activate_takeover(
        handle.socket_path(),
        Target::Claude,
        &user_home,
        ProviderFixture {
            name: "Adopt peer",
            base_url: "https://adopt-peer.example/v1",
            model: "adopt-peer-model",
            authentication: "anthropic-api-key",
            secret: "ADOPT_PEER_SECRET_96203",
        },
    )
    .await;
    let peer_endpoint = activation.model_endpoint_for(Target::Claude).await.unwrap();
    let historical = store.target_view_for(Target::Codex).await.unwrap();
    let historical_provider_id = historical.current_provider_id.clone().unwrap();
    let historical_snapshot = historical.activated_snapshot.clone().unwrap();
    let managed_bytes = fs::read(user_home.join(".codex/config.toml")).unwrap();
    let mut recursive = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut recursive).await;
    let _opened = request(
        &mut recursive,
        "recursive-open",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    let recursive_preview = request(
        &mut recursive,
        "recursive-preview",
        json!({"kind":"preview-reconciliation","target":"codex","strategy":"adopt"}),
    )
    .await;
    let recursive_rejected = request(
        &mut recursive,
        "recursive-apply",
        json!({
            "kind":"act","target":"codex","actionId":Uuid::new_v4(),
            "expectedRevision":historical.management_revision,
            "action":{
                "kind":"reconcile","strategy":"adopt",
                "observationToken":recursive_preview["result"]["preview"]["observationToken"],
                "acknowledgeVersion":"codex-cli 77.1.0"
            }
        }),
    )
    .await;
    assert_eq!(
        recursive_rejected["problem"]["code"],
        "stale-reconciliation-preview"
    );
    assert_eq!(
        fs::read(user_home.join(".codex/config.toml")).unwrap(),
        managed_bytes
    );
    assert!(activation.model_endpoint_for(Target::Codex).await.is_some());
    drop(recursive);
    fs::write(
        user_home.join(".codex/config.toml"),
        r#"model = "adopted-model"
model_provider = "muxvia_codex"
operator_setting = "preserved"

[model_providers.muxvia_codex]
name = "Externally adopted"
base_url = "https://adopted.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer ADOPTED_SECRET_96202" }
supports_websockets = false
"#,
    )
    .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    let revision = opened["result"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let preview = request(
        &mut stream,
        "preview",
        json!({"kind":"preview-reconciliation","target":"codex","strategy":"adopt"}),
    )
    .await;
    assert_eq!(preview["result"]["preview"]["restartRequired"], false);
    assert!(
        preview["result"]["preview"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change == &json!({"field":"takeover","state":"absent"}))
    );
    let token = preview["result"]["preview"]["observationToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let action_id = Uuid::new_v4();
    let operation = json!({
        "kind":"act","target":"codex","actionId":action_id,
        "expectedRevision":revision,
        "action":{"kind":"reconcile","strategy":"adopt","observationToken":token,
            "acknowledgeVersion":"codex-cli 77.1.0"}
    });
    let raw = request_raw(&mut stream, "apply", operation.clone()).await;
    assert_raw_frame_is_safe(&raw, &["HISTORICAL_SECRET_96201", "ADOPTED_SECRET_96202"]);
    let applied: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(applied["type"], "response");
    let view = &applied["result"]["outcome"]["view"];
    assert_eq!(view["managementRevision"], revision + 1);
    assert_eq!(view["providers"].as_array().unwrap().len(), 2);
    assert_eq!(view["providers"][0]["id"], historical_provider_id);
    assert_ne!(view["currentProviderId"], historical_provider_id);
    assert_ne!(
        view["activatedSnapshot"]["id"],
        historical_snapshot.id.to_string()
    );
    assert_eq!(view["mode"], "direct");
    assert_eq!(view["takeover"]["state"], "inactive");
    let _push = read_frame(&mut stream).await.unwrap();
    assert_eq!(activation.model_endpoint_for(Target::Codex).await, None);
    assert_eq!(
        activation.model_endpoint_for(Target::Claude).await,
        Some(peer_endpoint)
    );
    assert!(TcpStream::connect(peer_endpoint).await.is_ok());

    let replay = request(&mut stream, "replay", operation).await;
    assert_eq!(replay["result"]["outcome"]["status"], "replayed");
    assert_eq!(
        replay["result"]["outcome"]["view"]["managementRevision"],
        revision + 1
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut stream))
            .await
            .is_err(),
        "receipt replay published a second Target View"
    );

    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn restore_reconciliation_exits_only_the_idle_target_and_retains_provider_history() {
    let root = std::path::PathBuf::from("/tmp").join(format!(
        "mx-restore-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(CommandCodexProbe),
            probe_executable(&root, Target::Codex),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(
            Arc::new(CommandClaudeProbe),
            probe_executable(&root, Target::Claude),
        ),
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
            name: "Codex history",
            base_url: "https://codex-history.example/v1",
            model: "codex-history-model",
            authentication: "openai-bearer",
            secret: "CODEX_HISTORY_SECRET_96301",
        },
    )
    .await;
    activate_takeover(
        handle.socket_path(),
        Target::Claude,
        &user_home,
        ProviderFixture {
            name: "Claude peer",
            base_url: "https://claude-peer.example/v1",
            model: "claude-peer-model",
            authentication: "anthropic-api-key",
            secret: "CLAUDE_PEER_SECRET_96302",
        },
    )
    .await;
    let codex_endpoint = activation.model_endpoint_for(Target::Codex).await.unwrap();
    let claude_endpoint = activation.model_endpoint_for(Target::Claude).await.unwrap();
    let provider_count = store
        .target_view_for(Target::Codex)
        .await
        .unwrap()
        .providers
        .len();
    let mut drifted = fs::read_to_string(user_home.join(".codex/config.toml")).unwrap();
    drifted = format!(
        "restore_unrelated = \"keep-this\"\n{}",
        drifted.replace("codex-history-model", "restore-drift-model")
    );
    fs::write(user_home.join(".codex/config.toml"), drifted).unwrap();
    store
        .mark_configuration_drift_for(Target::Codex)
        .await
        .unwrap();

    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    let revision = opened["result"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let preview = request(
        &mut stream,
        "preview",
        json!({"kind":"preview-reconciliation","target":"codex","strategy":"restore"}),
    )
    .await;
    let token = preview["result"]["preview"]["observationToken"]
        .as_str()
        .unwrap();
    let restored = request(
        &mut stream,
        "apply",
        json!({
            "kind":"act","target":"codex","actionId":Uuid::new_v4(),
            "expectedRevision":revision,
            "action":{"kind":"reconcile","strategy":"restore","observationToken":token,
                "acknowledgeVersion":"codex-cli 77.1.0"}
        }),
    )
    .await;
    assert_eq!(restored["type"], "response", "{restored}");
    let view = &restored["result"]["outcome"]["view"];
    assert_eq!(view["takeover"]["state"], "inactive");
    assert!(view["currentProviderId"].is_null());
    assert!(view["activatedSnapshot"].is_null());
    assert_eq!(view["providers"].as_array().unwrap().len(), provider_count);
    let rendered = fs::read_to_string(user_home.join(".codex/config.toml")).unwrap();
    assert!(rendered.contains("restore_unrelated = \"keep-this\""));
    assert!(!rendered.contains("restore-drift-model"));
    let _push = read_frame(&mut stream).await.unwrap();
    assert_eq!(activation.model_endpoint_for(Target::Codex).await, None);
    assert_eq!(
        activation.model_endpoint_for(Target::Claude).await,
        Some(claude_endpoint)
    );
    assert!(TcpStream::connect(codex_endpoint).await.is_err());
    assert!(TcpStream::connect(claude_endpoint).await.is_ok());

    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn restore_returns_target_busy_without_mutation_while_real_request_is_pinned() {
    let root = std::path::PathBuf::from("/tmp").join(format!(
        "mx-busy-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let upstream = Arc::new(HeldUpstream::new());
    let activation = Arc::new(ActivationService::new(
        Arc::clone(&store),
        home.clone(),
        Arc::new(CommandCodexProbe),
        probe_executable(&root, Target::Codex),
        upstream.clone(),
    ));
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
            name: "Pinned provider",
            base_url: "https://pinned.example/v1",
            model: "pinned-model",
            authentication: "openai-bearer",
            secret: "PINNED_PROVIDER_SECRET_96401",
        },
    )
    .await;
    let endpoint = activation.model_endpoint_for(Target::Codex).await.unwrap();
    let routing_credential = store
        .routing_credential_for(Target::Codex)
        .await
        .unwrap()
        .unwrap();
    let request_started = upstream.started.notified();
    let request_task = tokio::spawn({
        let routing_credential = routing_credential.expose_secret().to_owned();
        async move {
            reqwest::Client::new()
                .post(format!("http://{endpoint}/v1/responses"))
                .header("X-Muxvia-Routing-Credential", routing_credential)
                .body("{}")
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        }
    });
    tokio::time::timeout(Duration::from_secs(2), request_started)
        .await
        .unwrap();
    fs::write(
        user_home.join(".codex/config.toml"),
        format!(
            "busy_unrelated = \"keep\"\n{}",
            fs::read_to_string(user_home.join(".codex/config.toml"))
                .unwrap()
                .replace("pinned-model", "busy-drift-model")
        ),
    )
    .unwrap();
    store
        .mark_configuration_drift_for(Target::Codex)
        .await
        .unwrap();
    let before_view = store.target_view_for(Target::Codex).await.unwrap();
    let before_file = fs::read(user_home.join(".codex/config.toml")).unwrap();

    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    let revision = opened["result"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let adopt_preview = request(
        &mut stream,
        "busy-adopt-preview",
        json!({"kind":"preview-reconciliation","target":"codex","strategy":"adopt"}),
    )
    .await;
    let busy_adopt = request(
        &mut stream,
        "busy-adopt",
        json!({
            "kind":"act","target":"codex","actionId":Uuid::new_v4(),
            "expectedRevision":revision,
            "action":{
                "kind":"reconcile","strategy":"adopt",
                "observationToken":adopt_preview["result"]["preview"]["observationToken"],
                "acknowledgeVersion":"codex-cli 77.1.0"
            }
        }),
    )
    .await;
    assert_eq!(busy_adopt["problem"]["code"], "target-busy");
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        before_view
    );
    assert_eq!(
        fs::read(user_home.join(".codex/config.toml")).unwrap(),
        before_file
    );
    let preview = request(
        &mut stream,
        "preview",
        json!({"kind":"preview-reconciliation","target":"codex","strategy":"restore"}),
    )
    .await;
    let token = preview["result"]["preview"]["observationToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let action_id = Uuid::new_v4();
    let operation = json!({
        "kind":"act","target":"codex","actionId":action_id,"expectedRevision":revision,
        "action":{"kind":"reconcile","strategy":"restore","observationToken":token,
            "acknowledgeVersion":"codex-cli 77.1.0"}
    });
    let busy = request(&mut stream, "busy", operation.clone()).await;
    assert_eq!(busy["type"], "error");
    assert_eq!(busy["problem"]["code"], "target-busy");
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        before_view
    );
    assert_eq!(
        fs::read(user_home.join(".codex/config.toml")).unwrap(),
        before_file
    );
    assert!(
        store
            .receipt_for(Target::Codex, action_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(TcpStream::connect(endpoint).await.is_ok());

    upstream.release.notify_one();
    assert_eq!(
        request_task.await.unwrap(),
        axum::body::Bytes::from_static(b"{}")
    );
    assert_eq!(
        upstream.urls.lock().await.as_slice(),
        ["https://pinned.example/v1/responses"]
    );
    let serving_push = read_frame(&mut stream).await.unwrap();
    assert_eq!(serving_push["type"], "target-view");
    let restored = request(&mut stream, "retry", operation).await;
    assert_eq!(restored["type"], "response", "{restored}");
    let _push = read_frame(&mut stream).await.unwrap();

    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn adopt_claude_supports_api_key_and_bearer_authentication_shapes() {
    for (suffix, key, expected_authentication) in [
        ("api-key", "ANTHROPIC_API_KEY", "anthropic-api-key"),
        ("bearer", "ANTHROPIC_AUTH_TOKEN", "anthropic-bearer"),
    ] {
        let root = std::path::PathBuf::from("/tmp").join(format!(
            "mx-claude-adopt-{suffix}-{}",
            &Uuid::new_v4().simple().to_string()[..8]
        ));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(CommandCodexProbe),
                probe_executable(&root, Target::Codex),
                Arc::new(NoopUpstream),
            )
            .with_claude_runtime(
                Arc::new(CommandClaudeProbe),
                probe_executable(&root, Target::Claude),
            ),
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
            Target::Claude,
            &user_home,
            ProviderFixture {
                name: "Historical Claude",
                base_url: "https://historical-claude.example/v1",
                model: "historical-claude-model",
                authentication: "anthropic-api-key",
                secret: "HISTORICAL_CLAUDE_SECRET_96501",
            },
        )
        .await;
        fs::write(
            user_home.join(".claude/settings.json"),
            serde_json::to_vec_pretty(&json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://adopted-claude.example/v1",
                    "ANTHROPIC_MODEL": "adopted-claude-model",
                    (key): format!("ADOPTED_CLAUDE_SECRET_{suffix}"),
                    "UNRELATED": "preserved"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        store
            .mark_configuration_drift_for(Target::Claude)
            .await
            .unwrap();
        let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut stream).await;
        let opened = request(
            &mut stream,
            "open",
            open_operation(Target::Claude, &user_home),
        )
        .await;
        let revision = opened["result"]["view"]["managementRevision"]
            .as_u64()
            .unwrap();
        let preview = request(
            &mut stream,
            "preview",
            json!({"kind":"preview-reconciliation","target":"claude","strategy":"adopt"}),
        )
        .await;
        let token = preview["result"]["preview"]["observationToken"]
            .as_str()
            .unwrap();
        let action_id = Uuid::new_v4();
        let operation = json!({
            "kind":"act","target":"claude","actionId":Uuid::new_v4(),
            "expectedRevision":revision,
            "action":{"kind":"reconcile","strategy":"adopt","observationToken":token,
                "acknowledgeVersion":"77.1.0 (Claude Code)"}
        });
        let mut operation = operation;
        operation["actionId"] = json!(action_id);
        let applied = request(&mut stream, "apply", operation.clone()).await;
        assert_eq!(applied["type"], "response", "{applied}");
        let view = &applied["result"]["outcome"]["view"];
        assert_eq!(view["providers"].as_array().unwrap().len(), 2);
        let current_id = view["currentProviderId"].as_str().unwrap();
        let current = view["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["id"] == current_id)
            .unwrap();
        assert_eq!(current["authentication"], expected_authentication);
        let rendered: Value =
            serde_json::from_slice(&fs::read(user_home.join(".claude/settings.json")).unwrap())
                .unwrap();
        assert_eq!(rendered["env"]["UNRELATED"], "preserved");
        let _push = read_frame(&mut stream).await.unwrap();
        let mut replay_stream = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut replay_stream).await;
        let opened_without_context = request(
            &mut replay_stream,
            "open-without-context",
            json!({"kind":"open-target","target":"claude"}),
        )
        .await;
        assert_eq!(opened_without_context["type"], "response");
        let replayed = request(&mut replay_stream, "replay-without-context", operation).await;
        assert_eq!(replayed["result"]["outcome"]["status"], "replayed");

        handle.shutdown().await.unwrap();
        activation.shutdown_models().await.unwrap();
        let _ = fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn claude_reapply_refreshes_unrelated_recovery_expectation_across_restart() {
    let root = std::path::PathBuf::from("/tmp").join(format!(
        "mx-claude-reapply-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let codex_probe = probe_executable(&root, Target::Codex);
    let claude_probe = probe_executable(&root, Target::Claude);
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(CommandCodexProbe),
            codex_probe.clone(),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), claude_probe.clone()),
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
        Target::Claude,
        &user_home,
        ProviderFixture {
            name: "Claude committed",
            base_url: "https://claude-committed.example/v1",
            model: "claude-committed-model",
            authentication: "anthropic-api-key",
            secret: "CLAUDE_REAPPLY_SECRET_96701",
        },
    )
    .await;
    let mut document: Value =
        serde_json::from_slice(&fs::read(user_home.join(".claude/settings.json")).unwrap())
            .unwrap();
    document["env"]["ANTHROPIC_MODEL"] = json!("claude-external-model");
    document["operatorUnrelated"] = json!({"latest": true});
    fs::write(
        user_home.join(".claude/settings.json"),
        serde_json::to_vec_pretty(&document).unwrap(),
    )
    .unwrap();
    store
        .mark_configuration_drift_for(Target::Claude)
        .await
        .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open",
        open_operation(Target::Claude, &user_home),
    )
    .await;
    let revision = opened["result"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let preview = request(
        &mut stream,
        "preview",
        json!({"kind":"preview-reconciliation","target":"claude","strategy":"reapply"}),
    )
    .await;
    let token = preview["result"]["preview"]["observationToken"]
        .as_str()
        .unwrap();
    let applied = request(
        &mut stream,
        "apply",
        json!({
            "kind":"act","target":"claude","actionId":Uuid::new_v4(),
            "expectedRevision":revision,
            "action":{"kind":"reconcile","strategy":"reapply","observationToken":token,
                "acknowledgeVersion":"77.1.0 (Claude Code)"}
        }),
    )
    .await;
    assert_eq!(applied["type"], "response", "{applied}");
    let _push = read_frame(&mut stream).await.unwrap();
    drop(stream);
    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();

    let restarted = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(CommandCodexProbe),
            codex_probe,
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), claude_probe),
    );
    let restarted_handle = ControlServer::bind_with_activation(
        &home,
        Arc::clone(&store),
        "test",
        Arc::clone(&restarted),
    )
    .await
    .unwrap();
    assert!(restarted.model_endpoint_for(Target::Claude).await.is_some());
    let view = store.target_view_for(Target::Claude).await.unwrap();
    assert!(!view.problems.iter().any(|problem| {
        matches!(
            problem.code.as_str(),
            "configuration-drift" | "recovery-required" | "startup-reconciliation-failed"
        )
    }));
    let document: Value =
        serde_json::from_slice(&fs::read(user_home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(document["env"]["ANTHROPIC_MODEL"], "claude-committed-model");
    assert_eq!(document["operatorUnrelated"], json!({"latest": true}));

    restarted_handle.shutdown().await.unwrap();
    restarted.shutdown_models().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn ordinary_write_gates_are_target_local_while_read_only_operations_remain_available() {
    let root = std::path::PathBuf::from("/tmp").join(format!(
        "mx-write-gates-{}",
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
            codex_executable.clone(),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), claude_executable.clone()),
    );
    let handle = ControlServer::bind_with_activation(
        &home,
        Arc::clone(&store),
        "test",
        Arc::clone(&activation),
    )
    .await
    .unwrap();
    rewrite_probe_as_incompatible(&codex_executable, Target::Codex);
    let mut unmanaged = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut unmanaged).await;
    let unmanaged_view = request(
        &mut unmanaged,
        "unmanaged-open",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    let unmanaged_blocked = request(
        &mut unmanaged,
        "unmanaged-incompatible-save",
        json!({
            "kind":"act","target":"codex","actionId":Uuid::new_v4(),
            "expectedRevision":unmanaged_view["result"]["view"]["managementRevision"],
            "action":{
                "kind":"create-provider","name":"blocked unmanaged",
                "baseUrl":"https://blocked-unmanaged.test/v1","model":"blocked",
                "credential":{"kind":"replace","value":"BLOCKED_UNMANAGED_SECRET_96700"},
                "authentication":"openai-bearer","presetKey":null
            }
        }),
    )
    .await;
    assert_eq!(
        unmanaged_blocked["problem"]["code"],
        "incompatible-target-cli"
    );
    drop(unmanaged);
    let _ = probe_executable(&root, Target::Codex);
    activate_takeover(
        handle.socket_path(),
        Target::Codex,
        &user_home,
        ProviderFixture {
            name: "Drift-gated Codex",
            base_url: "https://codex-gate.example/v1",
            model: "codex-gate-model",
            authentication: "openai-bearer",
            secret: "CODEX_GATE_SECRET_96701",
        },
    )
    .await;
    activate_takeover(
        handle.socket_path(),
        Target::Claude,
        &user_home,
        ProviderFixture {
            name: "Peer Claude",
            base_url: "https://claude-gate.example/v1",
            model: "claude-gate-model",
            authentication: "anthropic-api-key",
            secret: "CLAUDE_GATE_SECRET_96702",
        },
    )
    .await;
    let codex_path = user_home.join(".codex/config.toml");
    let codex_drifted = fs::read_to_string(&codex_path)
        .unwrap()
        .replace("codex-gate-model", "externally-drifted-model");
    fs::write(&codex_path, codex_drifted).unwrap();
    let codex_view = store.target_view_for(Target::Codex).await.unwrap();
    let codex_provider_id = codex_view.providers[0].id;
    let mut codex = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut codex).await;
    let opened = request(
        &mut codex,
        "open-codex",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    assert_eq!(opened["type"], "response");
    for (index, (request_id, action)) in [
        (
            "drift-save",
            json!({
                "kind":"create-provider","name":"blocked","baseUrl":"https://blocked.test/v1",
                "model":"blocked","credential":{"kind":"replace","value":"BLOCKED_SECRET_96703"},
                "authentication":"openai-bearer","presetKey":null
            }),
        ),
        (
            "drift-activate",
            json!({
                "kind":"activate-provider","providerId":codex_provider_id,
                "mode":"direct"
            }),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let blocked = request(
            &mut codex,
            request_id,
            json!({
                "kind":"act","target":"codex","actionId":Uuid::new_v4(),
                "expectedRevision":codex_view.management_revision,"action":action
            }),
        )
        .await;
        assert_eq!(blocked["problem"]["code"], "configuration-drift");
        assert!(
            blocked["authoritativeView"]["problems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|problem| problem["code"] == "configuration-drift")
        );
        if index == 0 {
            let pushed = read_frame(&mut codex).await.unwrap();
            assert_eq!(pushed["type"], "target-view");
            assert!(
                pushed["view"]["problems"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|problem| problem["code"] == "configuration-drift")
            );
        }
    }
    assert!(
        store
            .target_view_for(Target::Codex)
            .await
            .unwrap()
            .problems
            .iter()
            .any(|problem| problem.code == "configuration-drift")
    );
    let drift_preview = request(
        &mut codex,
        "drift-preview",
        json!({"kind":"preview-reconciliation","target":"codex","strategy":"reapply"}),
    )
    .await;
    assert_eq!(drift_preview["type"], "response");

    let mut claude = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut claude).await;
    let opened = request(
        &mut claude,
        "open-claude",
        open_operation(Target::Claude, &user_home),
    )
    .await;
    let claude_revision = opened["result"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let unacknowledged = request(
        &mut claude,
        "peer-unacknowledged",
        json!({
            "kind":"act","target":"claude","actionId":Uuid::new_v4(),
            "expectedRevision":claude_revision,
            "action":{
                "kind":"create-provider","name":"blocked unknown","baseUrl":"https://blocked-unknown.test/v1",
                "model":"blocked","credential":{"kind":"replace","value":"BLOCKED_UNKNOWN_SECRET_96706"},
                "authentication":"anthropic-api-key","presetKey":null
            }
        }),
    )
    .await;
    assert_eq!(
        unacknowledged["problem"]["code"],
        "compatibility-acknowledgement-required"
    );
    store
        .record_compatibility(
            Target::Claude,
            "77.1.0 (Claude Code)".into(),
            muxvia_routing::control::protocol::CompatibilityClassification::UnknownCompatible,
        )
        .await
        .unwrap();
    store
        .acknowledge_compatibility(Target::Claude, "77.1.0 (Claude Code)")
        .await
        .unwrap();
    let peer_saved = request(
        &mut claude,
        "peer-save",
        json!({
            "kind":"act","target":"claude","actionId":Uuid::new_v4(),
            "expectedRevision":claude_revision,
            "action":{
                "kind":"create-provider","name":"peer allowed","baseUrl":"https://peer-allowed.test/v1",
                "model":"peer-allowed","credential":{"kind":"replace","value":"PEER_ALLOWED_SECRET_96704"},
                "authentication":"anthropic-api-key","presetKey":null
            }
        }),
    )
    .await;
    assert_eq!(peer_saved["type"], "response");
    let _peer_push = read_frame(&mut claude).await.unwrap();

    rewrite_probe_as_incompatible(&claude_executable, Target::Claude);
    let claude_view = store.target_view_for(Target::Claude).await.unwrap();
    let claude_provider_id = claude_view.providers[0].id;
    for (request_id, action) in [
        (
            "incompatible-save",
            json!({
                "kind":"create-provider","name":"blocked","baseUrl":"https://blocked.test/v1",
                "model":"blocked","credential":{"kind":"replace","value":"BLOCKED_SECRET_96705"},
                "authentication":"anthropic-api-key","presetKey":null
            }),
        ),
        (
            "incompatible-activate",
            json!({
                "kind":"activate-provider","providerId":claude_provider_id,
                "mode":"direct"
            }),
        ),
    ] {
        let blocked = request(
            &mut claude,
            request_id,
            json!({
                "kind":"act","target":"claude","actionId":Uuid::new_v4(),
                "expectedRevision":claude_view.management_revision,"action":action
            }),
        )
        .await;
        assert_eq!(blocked["problem"]["code"], "incompatible-target-cli");
    }
    let incompatible_preview = request(
        &mut claude,
        "incompatible-preview",
        json!({"kind":"preview-reconciliation","target":"claude","strategy":"reapply"}),
    )
    .await;
    assert_eq!(incompatible_preview["type"], "response");
    assert_eq!(
        incompatible_preview["result"]["preview"]["compatibility"]["classification"],
        "incompatible"
    );
    let missing_provider = Uuid::new_v4();
    for (request_id, operation) in [
        (
            "incompatible-discovery",
            json!({
                "kind":"discover-models","target":"claude",
                "source":{"kind":"saved","providerId":missing_provider,"providerRevision":1}
            }),
        ),
        (
            "incompatible-reachability",
            json!({
                "kind":"check-reachability","target":"claude",
                "providerId":missing_provider,"providerRevision":1
            }),
        ),
    ] {
        let inspected = request(&mut claude, request_id, operation).await;
        assert_ne!(inspected["problem"]["code"], "incompatible-target-cli");
        assert_ne!(inspected["problem"]["code"], "configuration-drift");
    }

    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();
    let _ = fs::remove_dir_all(root);
}
