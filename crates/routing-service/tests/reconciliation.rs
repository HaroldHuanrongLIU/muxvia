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
    claude::{ClaudeConfigCodec, ClaudeConfigSnapshot, CommandClaudeProbe, DesiredClaudeState},
    codex::{CodexConfigCodec, CommandCodexProbe, ConfigSnapshot, DesiredCodexState},
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

#[derive(Clone, PartialEq, Eq)]
struct SecretFileFingerprint {
    length: usize,
    sha256: [u8; 32],
}

fn secret_file_fingerprint(path: &Path) -> SecretFileFingerprint {
    let bytes = fs::read(path).unwrap();
    SecretFileFingerprint {
        length: bytes.len(),
        sha256: ring::digest::digest(&ring::digest::SHA256, &bytes)
            .as_ref()
            .try_into()
            .unwrap(),
    }
}

fn assert_secret_file_unchanged(path: &Path, expected: &SecretFileFingerprint) {
    assert!(
        secret_file_fingerprint(path) == *expected,
        "secret-bearing artifact changed unexpectedly"
    );
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic".to_owned())
}

#[test]
fn secret_file_comparison_uses_fixed_redacted_diagnostics_on_both_branches() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("managed-config");
    let before_secret = "BEFORE_COMPARISON_SECRET_98402";
    let after_secret = "AFTER_COMPARISON_SECRET_98403";
    fs::write(&path, before_secret).unwrap();
    let expected = secret_file_fingerprint(&path);
    assert_secret_file_unchanged(&path, &expected);
    fs::write(&path, after_secret).unwrap();
    let diagnostic = panic_text(
        std::panic::catch_unwind(|| assert_secret_file_unchanged(&path, &expected)).unwrap_err(),
    );
    for sentinel in [before_secret, after_secret] {
        assert!(!diagnostic.contains(sentinel));
        assert!(!diagnostic.contains(&format!("{:?}", sentinel.as_bytes())));
    }
    assert_eq!(diagnostic, "secret-bearing artifact changed unexpectedly");
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
    let mut activated = request(
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
    if activated["type"] == "error" {
        assert_eq!(
            activated["problem"]["code"],
            "compatibility-acknowledgement-required"
        );
        let _blocker_push = read_frame(&mut stream).await.unwrap();
        let store = StateStore::open(&MuxviaHome::from_user_home(user_home))
            .await
            .unwrap();
        let compatibility = store.compatibility_for(target).await.unwrap();
        store
            .acknowledge_compatibility(target, &compatibility.version)
            .await
            .unwrap();
        activated = request(
            &mut stream,
            "acknowledged-activate",
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
    }
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

async fn managed_config_version_for(home: &MuxviaHome, target: Target) -> u32 {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| {
            connection.query_row(
                "SELECT managed_config_version FROM target_route_state WHERE target = ?1",
                [target.as_str()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap()
}

async fn seed_pending_reconciliation_intent(
    home: &MuxviaHome,
    target: Target,
    action_id: Uuid,
    before_json: String,
    desired_json: String,
) {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| {
            connection.execute(
                "INSERT INTO reconciliation_intents
                 (action_id, target, strategy, state, created_revision, before_json, desired_json)
                 VALUES (?1, ?2, 'reapply', 'pending',
                   (SELECT management_revision FROM target_route_state WHERE target = ?2), ?3, ?4)",
                tokio_rusqlite::rusqlite::params![
                    action_id.to_string(),
                    target.as_str(),
                    before_json,
                    desired_json
                ],
            )?;
            Ok::<(), tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
}

async fn committed_recovery_desired_json(home: &MuxviaHome, target: Target) -> String {
    let payload = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| {
            connection.query_row(
                "SELECT a.payload_json FROM target_route_state r
                 JOIN activation_recovery a ON a.id = r.recovery_intent_id
                 WHERE r.target = ?1",
                [target.as_str()],
                |row| row.get::<_, String>(0),
            )
        })
        .await
        .unwrap();
    serde_json::from_str::<Value>(&payload).unwrap()["desired"].to_string()
}

#[tokio::test]
async fn startup_recovers_pending_reconciliation_intents_for_both_targets_and_crash_boundaries() {
    for (target, after_write) in [
        (Target::Codex, false),
        (Target::Codex, true),
        (Target::Claude, false),
        (Target::Claude, true),
    ] {
        let root = PathBuf::from("/tmp").join(format!(
            "mx-reconcile-restart-{}-{}-{}",
            target.as_str(),
            if after_write { "write" } else { "intent" },
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
            target,
            &user_home,
            ProviderFixture {
                name: "Restart provider",
                base_url: match target {
                    Target::Codex => "https://restart-codex.example/v1",
                    Target::Claude => "https://restart-claude.example/v1",
                },
                model: "restart-model",
                authentication: match target {
                    Target::Codex => "openai-bearer",
                    Target::Claude => "anthropic-api-key",
                },
                secret: "RESTART_PROVIDER_SECRET_97106",
            },
        )
        .await;
        let desired_json = committed_recovery_desired_json(&home, target).await;
        let before_json = match target {
            Target::Codex => {
                let codec = CodexConfigCodec::for_user_home(&user_home).unwrap();
                fs::write(
                    codec.config_path(),
                    r#"model = "restart-drift-model"
model_provider = "restart_external"
[model_providers.restart_external]
name = "Restart external"
base_url = "https://restart-drift.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer RESTART_DRIFT_SECRET_97107" }
supports_websockets = false
"#,
                )
                .unwrap();
                let before = codec.inspect().unwrap();
                let json = serde_json::to_string(&before).unwrap();
                if after_write {
                    let desired: DesiredCodexState = serde_json::from_str(&desired_json).unwrap();
                    codec.atomic_apply(&before, &desired).unwrap();
                }
                json
            }
            Target::Claude => {
                let codec = ClaudeConfigCodec::for_user_home(&user_home).unwrap();
                fs::write(
                    codec.settings_path(),
                    serde_json::to_vec_pretty(&json!({
                        "env": {
                            "ANTHROPIC_BASE_URL": "https://restart-drift.example/v1",
                            "ANTHROPIC_MODEL": "restart-drift-model",
                            "ANTHROPIC_AUTH_TOKEN": "RESTART_DRIFT_SECRET_97108",
                            "OPERATOR_SETTING": "preserve"
                        }
                    }))
                    .unwrap(),
                )
                .unwrap();
                let before = codec.inspect().unwrap();
                let json = serde_json::to_string(&before).unwrap();
                if after_write {
                    let desired: DesiredClaudeState = serde_json::from_str(&desired_json).unwrap();
                    codec.atomic_apply(&before, &desired).unwrap();
                }
                json
            }
        };
        let action_id = Uuid::new_v4();
        seed_pending_reconciliation_intent(
            &home,
            target,
            action_id,
            before_json.clone(),
            desired_json.clone(),
        )
        .await;
        let provider_count = store.target_view_for(target).await.unwrap().providers.len();

        handle.shutdown().await.unwrap();
        activation.shutdown_models().await.unwrap();
        drop(activation);
        drop(store);

        let reopened = Arc::new(StateStore::open(&home).await.unwrap());
        let restarted_activation = Arc::new(
            ActivationService::new(
                Arc::clone(&reopened),
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
        let restarted = ControlServer::bind_with_activation(
            &home,
            Arc::clone(&reopened),
            "test",
            Arc::clone(&restarted_activation),
        )
        .await
        .unwrap();
        assert_eq!(
            reopened.managed_write_status_for(target).await.unwrap(),
            muxvia_routing::state::ManagedWriteStatus::ConfigurationDrift
        );
        let intent_state = tokio_rusqlite::Connection::open(home.database_path())
            .await
            .unwrap()
            .call(move |connection| {
                connection.query_row(
                    "SELECT state FROM reconciliation_intents
                     WHERE target = ?1 AND action_id = ?2",
                    tokio_rusqlite::rusqlite::params![target.as_str(), action_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(intent_state, "rolled-back");
        assert!(
            reopened
                .receipt_for(target, action_id)
                .await
                .unwrap()
                .is_none()
        );
        match target {
            Target::Codex => {
                let before: ConfigSnapshot = serde_json::from_str(&before_json).unwrap();
                let desired: DesiredCodexState = serde_json::from_str(&desired_json).unwrap();
                CodexConfigCodec::for_user_home(&user_home)
                    .unwrap()
                    .restore_or_confirm_before(&before, &desired)
                    .unwrap();
            }
            Target::Claude => {
                let before: ClaudeConfigSnapshot = serde_json::from_str(&before_json).unwrap();
                let desired: DesiredClaudeState = serde_json::from_str(&desired_json).unwrap();
                ClaudeConfigCodec::for_user_home(&user_home)
                    .unwrap()
                    .restore_or_confirm_before(&before, &desired)
                    .unwrap();
            }
        }

        let mut stream = UnixStream::connect(restarted.socket_path()).await.unwrap();
        hello(&mut stream).await;
        let opened = request(
            &mut stream,
            "restart-open",
            open_operation(target, &user_home),
        )
        .await;
        let preview = request(
            &mut stream,
            "restart-preview",
            json!({"kind":"preview-reconciliation","target":target,"strategy":"reapply"}),
        )
        .await;
        let applied = request(
            &mut stream,
            "restart-retry",
            json!({
                "kind":"act","target":target,"actionId":action_id,
                "expectedRevision":opened["result"]["view"]["managementRevision"],
                "action":{"kind":"reconcile","strategy":"reapply",
                    "observationToken":preview["result"]["preview"]["observationToken"],
                    "acknowledgeVersion": match target {
                        Target::Codex => "codex-cli 77.1.0",
                        Target::Claude => "77.1.0 (Claude Code)",
                    }}
            }),
        )
        .await;
        assert_eq!(applied["type"], "response", "{applied}");
        assert_eq!(
            applied["result"]["outcome"]["view"]["providers"]
                .as_array()
                .unwrap()
                .len(),
            provider_count
        );
        let _push = read_frame(&mut stream).await.unwrap();
        assert!(
            reopened
                .receipt_for(target, action_id)
                .await
                .unwrap()
                .is_some()
        );

        restarted.shutdown().await.unwrap();
        restarted_activation.shutdown_models().await.unwrap();
        let _ = fs::remove_dir_all(root);
    }
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

    let before_database = secret_file_fingerprint(home.database_path());
    let before_codex = secret_file_fingerprint(&user_home.join(".codex/config.toml"));
    let before_claude = secret_file_fingerprint(&user_home.join(".claude/settings.json"));
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
            assert_secret_file_unchanged(home.database_path(), &before_database);
            assert_secret_file_unchanged(&user_home.join(".codex/config.toml"), &before_codex);
            assert_secret_file_unchanged(&user_home.join(".claude/settings.json"), &before_claude);
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

    assert_secret_file_unchanged(home.database_path(), &before_database);
    assert_secret_file_unchanged(&user_home.join(".codex/config.toml"), &before_codex);
    assert_secret_file_unchanged(&user_home.join(".claude/settings.json"), &before_claude);
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
    let historical_configuration = r#"# bound historical configuration
model = "historical-local-model" # historical model decoration
model_provider = "operator_openai" # historical selector decoration
operator_setting = "historical-unrelated"

[model_providers.operator_openai]
name = "Historical local provider" # historical provider decoration
base_url = "https://historical-local.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer HISTORICAL_LOCAL_SECRET_97308" }
supports_websockets = false
operator_note = "preserve-this-unrelated-field"
"#;
    fs::create_dir_all(user_home.join(".codex")).unwrap();
    fs::write(
        user_home.join(".codex/config.toml"),
        historical_configuration,
    )
    .unwrap();
    fs::set_permissions(
        user_home.join(".codex/config.toml"),
        fs::Permissions::from_mode(0o640),
    )
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
    let route_credential = store
        .routing_credential_for(Target::Codex)
        .await
        .unwrap()
        .unwrap();
    let historical_provider_id = historical.current_provider_id.clone().unwrap();
    let historical_snapshot = historical.activated_snapshot.clone().unwrap();
    let route_secret_configuration = format!(
        r#"model = "must-not-adopt-route-secret"
model_provider = "external_route"

[model_providers.external_route]
name = "External route"
base_url = "https://externally-changed.example/v1"
wire_api = "responses"
http_headers = {{ "X-Muxvia-Routing-Credential" = {} }}
supports_websockets = false
"#,
        serde_json::to_string(route_credential.expose_secret()).unwrap()
    );
    fs::write(
        user_home.join(".codex/config.toml"),
        &route_secret_configuration,
    )
    .unwrap();
    let route_secret_bytes = fs::read(user_home.join(".codex/config.toml")).unwrap();
    let route_secret_view = store.target_view_for(Target::Codex).await.unwrap();
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
    let recursive_rejected_raw = request_raw(
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
    assert_raw_frame_is_safe(&recursive_rejected_raw, &[route_credential.expose_secret()]);
    let recursive_rejected: Value = serde_json::from_slice(&recursive_rejected_raw).unwrap();
    assert_eq!(
        recursive_rejected["problem"]["code"],
        "invalid-provider-credential"
    );
    assert!(
        fs::read(user_home.join(".codex/config.toml")).unwrap() == route_secret_bytes,
        "rejected Codex Adopt changed the Managed Configuration"
    );
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        route_secret_view
    );
    assert!(activation.model_endpoint_for(Target::Codex).await.is_some());
    let different_routing_header_secret =
        "DIFFERENT_STALE_ROUTING_HEADER_SECRET_97303_MUST_NOT_PERSIST_000000";
    let different_routing_header_configuration = format!(
        r#"model = "must-not-adopt-routing-header"
model_provider = "external_route"

[model_providers.external_route]
name = "External route"
base_url = "https://different-routing-header.example/v1"
wire_api = "responses"
http_headers = {{ "X-Muxvia-Routing-Credential" = {} }}
supports_websockets = false
"#,
        serde_json::to_string(different_routing_header_secret).unwrap()
    );
    fs::write(
        user_home.join(".codex/config.toml"),
        &different_routing_header_configuration,
    )
    .unwrap();
    let different_route_bytes = fs::read(user_home.join(".codex/config.toml")).unwrap();
    let different_route_view = store.target_view_for(Target::Codex).await.unwrap();
    let different_preview = request(
        &mut recursive,
        "different-route-preview",
        json!({"kind":"preview-reconciliation","target":"codex","strategy":"adopt"}),
    )
    .await;
    let different_rejected_raw = request_raw(
        &mut recursive,
        "different-route-apply",
        json!({
            "kind":"act","target":"codex","actionId":Uuid::new_v4(),
            "expectedRevision":historical.management_revision,
            "action":{"kind":"reconcile","strategy":"adopt",
                "observationToken":different_preview["result"]["preview"]["observationToken"],
                "acknowledgeVersion":"codex-cli 77.1.0"}
        }),
    )
    .await;
    assert_raw_frame_is_safe(&different_rejected_raw, &[different_routing_header_secret]);
    let different_rejected: Value = serde_json::from_slice(&different_rejected_raw).unwrap();
    assert_eq!(
        different_rejected["problem"]["code"],
        "invalid-provider-credential"
    );
    assert!(
        fs::read(user_home.join(".codex/config.toml")).unwrap() == different_route_bytes,
        "rejected routing-header Adopt changed the Managed Configuration"
    );
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        different_route_view
    );
    drop(recursive);
    fs::write(
        user_home.join(".codex/config.toml"),
        r#"model = "adopted-model"
model_provider = "operator_openai"
operator_setting = "preserved"
current_only_unrelated_secret = "CURRENT_ONLY_UNRELATED_SECRET_97310"

[model_providers.muxvia_codex]
name = "Stale Muxvia"
base_url = "https://stale-muxvia.invalid/v1"
wire_api = "chat"
http_headers = { Authorization = "Bearer STALE_MUXVIA_SECRET_96204" }
supports_websockets = true

[model_providers.operator_openai]
name = "Externally adopted" # current provider decoration
base_url = "https://adopted.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer ADOPTED_SECRET_96202" }
supports_websockets = false
operator_note = "preserve-this-unrelated-field"
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
    assert_raw_frame_is_safe(
        &raw,
        &[
            "HISTORICAL_SECRET_96201",
            "ADOPTED_SECRET_96202",
            "STALE_MUXVIA_SECRET_96204",
        ],
    );
    let applied: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(applied["type"], "response");
    let view = &applied["result"]["outcome"]["view"];
    assert_eq!(view["managementRevision"], revision + 1);
    assert_eq!(view["providers"].as_array().unwrap().len(), 2);
    assert_eq!(view["providers"][0]["id"], historical_provider_id);
    assert_ne!(view["currentProviderId"], historical_provider_id);
    let adopted_provider = view["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["id"] == view["currentProviderId"])
        .unwrap();
    assert_eq!(adopted_provider["name"], "Externally adopted");
    assert_eq!(adopted_provider["baseUrl"], "https://adopted.example/v1");
    assert_eq!(adopted_provider["model"], "adopted-model");
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
    let database = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap();
    let recovery_payload_json = database
        .call(|connection| {
            connection.query_row(
                "SELECT a.payload_json FROM target_route_state r
                 JOIN activation_recovery a
                   ON a.id = r.recovery_intent_id AND a.target = r.target
                 WHERE r.target = 'codex' AND a.state = 'committed'",
                [],
                |row| row.get::<_, String>(0),
            )
        })
        .await
        .unwrap();
    assert!(
        !recovery_payload_json.contains("CURRENT_ONLY_UNRELATED_SECRET_97310"),
        "Codex Adopt persisted a current-only unrelated secret in recovery payload JSON"
    );

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

    drop(stream);
    handle.shutdown().await.unwrap();
    let handle = ControlServer::bind_with_activation(
        &home,
        Arc::clone(&store),
        "test",
        Arc::clone(&activation),
    )
    .await
    .unwrap();
    let mut after_restart = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut after_restart).await;
    let reopened = request(
        &mut after_restart,
        "open-after-adopt-restart",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    let activated = request(
        &mut after_restart,
        "activate-after-adopt",
        json!({
            "kind":"act","target":"codex","actionId":Uuid::new_v4(),
            "expectedRevision":reopened["result"]["view"]["managementRevision"],
            "action":{"kind":"activate-provider","providerId":historical_provider_id,
                "mode":"takeover"}
        }),
    )
    .await;
    assert_eq!(
        activated["type"], "response",
        "post-Adopt activation failed with {:?}",
        activated["problem"]["code"]
    );
    let _push = read_frame(&mut after_restart).await.unwrap();
    let activated_configuration = fs::read_to_string(user_home.join(".codex/config.toml")).unwrap();
    assert!(activated_configuration.contains("model_provider = \"operator_openai\""));
    assert!(activated_configuration.contains("[model_providers.operator_openai]"));
    assert!(activated_configuration.contains("[model_providers.muxvia_codex]"));
    assert!(activated_configuration.contains("STALE_MUXVIA_SECRET_96204"));

    drop(after_restart);
    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();
    drop(activation);
    drop(store);
    let restarted_store = Arc::new(StateStore::open(&home).await.unwrap());
    let restarted_activation = Arc::new(
        ActivationService::new(
            Arc::clone(&restarted_store),
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
    let restarted = ControlServer::bind_with_activation(
        &home,
        Arc::clone(&restarted_store),
        "test",
        Arc::clone(&restarted_activation),
    )
    .await
    .unwrap();
    assert!(
        restarted_activation
            .model_endpoint_for(Target::Codex)
            .await
            .is_some()
    );
    assert!(
        fs::read_to_string(user_home.join(".codex/config.toml"))
            .is_ok_and(|current| current == activated_configuration),
        "restart changed the secret-bearing managed configuration"
    );

    let restore_drift = activated_configuration.replacen(
        "model = \"historical-model\"",
        "model = \"restore-after-restart-drift\"",
        1,
    );
    fs::write(user_home.join(".codex/config.toml"), restore_drift).unwrap();
    restarted_store
        .mark_configuration_drift_for(Target::Codex)
        .await
        .unwrap();
    let mut restore_stream = UnixStream::connect(restarted.socket_path()).await.unwrap();
    hello(&mut restore_stream).await;
    let restore_opened = request(
        &mut restore_stream,
        "open-before-historical-restore",
        open_operation(Target::Codex, &user_home),
    )
    .await;
    let restore_preview = request(
        &mut restore_stream,
        "preview-historical-restore",
        json!({"kind":"preview-reconciliation","target":"codex","strategy":"restore"}),
    )
    .await;
    let restore_response = request(
        &mut restore_stream,
        "apply-historical-restore",
        json!({
            "kind":"act","target":"codex","actionId":Uuid::new_v4(),
            "expectedRevision":restore_opened["result"]["view"]["managementRevision"],
            "action":{"kind":"reconcile","strategy":"restore",
                "observationToken":restore_preview["result"]["preview"]["observationToken"],
                "acknowledgeVersion":"codex-cli 77.1.0"}
        }),
    )
    .await;
    assert_eq!(restore_response["type"], "response", "{restore_response}");
    assert_eq!(
        restore_response["result"]["outcome"]["view"]["mode"], "unmanaged",
        "{restore_response}"
    );
    let _restore_push = read_frame(&mut restore_stream).await.unwrap();
    let restored_configuration = fs::read_to_string(user_home.join(".codex/config.toml")).unwrap();
    for expected in [
        "model = \"historical-local-model\" # historical model decoration",
        "model_provider = \"operator_openai\" # historical selector decoration",
        "name = \"Historical local provider\" # historical provider decoration",
        "base_url = \"https://historical-local.example/v1\"",
        "Bearer HISTORICAL_LOCAL_SECRET_97308",
        "operator_setting = \"preserved\"",
        "current_only_unrelated_secret = \"CURRENT_ONLY_UNRELATED_SECRET_97310\"",
        "operator_note = \"preserve-this-unrelated-field\"",
    ] {
        assert!(
            restored_configuration.contains(expected),
            "historical Restore lost an approved bounded field"
        );
    }
    assert!(!restored_configuration.contains("restore-after-restart-drift"));
    assert!(!restored_configuration.contains("[model_providers.muxvia_codex]"));
    assert!(!restored_configuration.contains("STALE_MUXVIA_SECRET_96204"));
    assert_eq!(
        fs::metadata(user_home.join(".codex/config.toml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    let restored_view = restarted_store
        .target_view_for(Target::Codex)
        .await
        .unwrap();
    assert_eq!(restored_view.mode, "unmanaged");
    assert_eq!(restored_view.recovery.state, "clean");
    assert!(!restored_view.problems.iter().any(|problem| {
        matches!(
            problem.code.as_str(),
            "recovery-required" | "startup-reconciliation-failed"
        )
    }));
    assert_eq!(
        restarted_activation.model_endpoint_for(Target::Codex).await,
        None
    );

    restarted.shutdown().await.unwrap();
    restarted_activation.shutdown_models().await.unwrap();
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
async fn claude_restore_reopens_as_valid_unmanaged_v1_without_changing_the_peer() {
    let root = std::path::PathBuf::from("/tmp").join(format!(
        "mx-claude-restore-reopen-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    ));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let codex_executable = probe_executable(&root, Target::Codex);
    let claude_executable = probe_executable(&root, Target::Claude);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
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
            name: "Restore peer",
            base_url: "https://restore-peer.example/v1",
            model: "restore-peer-model",
            authentication: "openai-bearer",
            secret: "RESTORE_PEER_SECRET_96311",
        },
    )
    .await;
    activate_takeover(
        handle.socket_path(),
        Target::Claude,
        &user_home,
        ProviderFixture {
            name: "Claude restored target",
            base_url: "https://claude-restored.example/v1",
            model: "claude-restored-model",
            authentication: "anthropic-api-key",
            secret: "CLAUDE_RESTORED_SECRET_96312",
        },
    )
    .await;
    assert_eq!(managed_config_version_for(&home, Target::Claude).await, 2);

    let peer_endpoint = activation.model_endpoint_for(Target::Codex).await.unwrap();
    let peer_configuration = fs::read(user_home.join(".codex/config.toml")).unwrap();
    let mut peer_view = store.target_view_for(Target::Codex).await.unwrap();
    let peer_credential = store
        .routing_credential_for(Target::Codex)
        .await
        .unwrap()
        .unwrap();
    let peer_snapshot = store
        .activated_snapshot_for(Target::Codex)
        .await
        .unwrap()
        .unwrap();
    let peer_snapshot_identity = (
        peer_snapshot.id(),
        peer_snapshot.provider_id(),
        peer_snapshot.base_url().to_owned(),
        peer_snapshot.model().to_owned(),
        peer_snapshot.protocol(),
        peer_snapshot.authentication(),
        peer_snapshot.epoch(),
    );
    let peer_provider_credential = peer_snapshot
        .provider_credential()
        .expose_secret()
        .to_owned();

    let settings_path = user_home.join(".claude/settings.json");
    let mut drifted: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    drifted["env"]["ANTHROPIC_MODEL"] = json!("claude-restore-external-model");
    drifted["restoreUnrelated"] = json!({"preserve": true});
    fs::write(&settings_path, serde_json::to_vec_pretty(&drifted).unwrap()).unwrap();
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
        json!({"kind":"preview-reconciliation","target":"claude","strategy":"restore"}),
    )
    .await;
    let restored = request(
        &mut stream,
        "apply",
        json!({
            "kind":"act","target":"claude","actionId":Uuid::new_v4(),
            "expectedRevision":revision,
            "action":{
                "kind":"reconcile","strategy":"restore",
                "observationToken":preview["result"]["preview"]["observationToken"],
                "acknowledgeVersion":"77.1.0 (Claude Code)"
            }
        }),
    )
    .await;
    assert_eq!(restored["type"], "response", "{restored}");
    assert_eq!(restored["result"]["outcome"]["view"]["mode"], "unmanaged");
    let _push = read_frame(&mut stream).await.unwrap();
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        peer_view
    );
    assert_eq!(
        activation.model_endpoint_for(Target::Codex).await,
        Some(peer_endpoint)
    );
    assert!(
        fs::read(user_home.join(".codex/config.toml")).unwrap() == peer_configuration,
        "Claude Restore changed the peer Managed Configuration"
    );

    drop(stream);
    handle.shutdown().await.unwrap();
    activation.shutdown_models().await.unwrap();
    drop(activation);
    drop(store);

    let reopened_store = Arc::new(StateStore::open(&home).await.unwrap());
    let reopened_activation = Arc::new(
        ActivationService::new(
            Arc::clone(&reopened_store),
            home.clone(),
            Arc::new(CommandCodexProbe),
            codex_executable,
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), claude_executable),
    );
    let reopened_handle = ControlServer::bind_with_activation(
        &home,
        Arc::clone(&reopened_store),
        "test",
        Arc::clone(&reopened_activation),
    )
    .await
    .unwrap();

    let reopened_claude = reopened_store
        .target_view_for(Target::Claude)
        .await
        .unwrap();
    assert_eq!(managed_config_version_for(&home, Target::Claude).await, 1);
    assert_eq!(reopened_claude.mode, "unmanaged");
    assert_eq!(reopened_claude.recovery.state, "clean");
    assert!(!reopened_claude.problems.iter().any(|problem| {
        matches!(
            problem.code.as_str(),
            "recovery-required" | "startup-reconciliation-failed" | "model-route-unavailable"
        )
    }));

    peer_view.service.epoch = reopened_store.service_epoch().to_string();
    assert_eq!(
        reopened_store.target_view_for(Target::Codex).await.unwrap(),
        peer_view
    );
    assert_eq!(
        reopened_activation.model_endpoint_for(Target::Codex).await,
        Some(peer_endpoint)
    );
    assert!(
        fs::read(user_home.join(".codex/config.toml")).unwrap() == peer_configuration,
        "Claude Restore changed the peer Managed Configuration after restart"
    );
    let reopened_peer_credential = reopened_store
        .routing_credential_for(Target::Codex)
        .await
        .unwrap()
        .unwrap();
    assert!(
        reopened_peer_credential.expose_secret() == peer_credential.expose_secret(),
        "Claude Restore changed the peer Routing Credential"
    );
    let reopened_peer_snapshot = reopened_store
        .activated_snapshot_for(Target::Codex)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            reopened_peer_snapshot.id(),
            reopened_peer_snapshot.provider_id(),
            reopened_peer_snapshot.base_url().to_owned(),
            reopened_peer_snapshot.model().to_owned(),
            reopened_peer_snapshot.protocol(),
            reopened_peer_snapshot.authentication(),
            reopened_peer_snapshot.epoch(),
        ),
        peer_snapshot_identity
    );
    assert!(
        reopened_peer_snapshot.provider_credential().expose_secret()
            == peer_provider_credential.as_str(),
        "Claude Restore changed the peer Provider Credential"
    );

    reopened_handle.shutdown().await.unwrap();
    reopened_activation.shutdown_models().await.unwrap();
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
    let before_file = secret_file_fingerprint(&user_home.join(".codex/config.toml"));

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
    assert_eq!(busy_adopt["problem"]["code"], "invalid-provider-credential");
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        before_view
    );
    assert_secret_file_unchanged(&user_home.join(".codex/config.toml"), &before_file);
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
    assert_secret_file_unchanged(&user_home.join(".codex/config.toml"), &before_file);
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
        let route_credential = store
            .routing_credential_for(Target::Claude)
            .await
            .unwrap()
            .unwrap();
        let route_secret_configuration = serde_json::to_vec_pretty(&json!({
            "env": {
                "ANTHROPIC_BASE_URL": "https://externally-changed-claude.example/v1",
                "ANTHROPIC_MODEL": "must-not-adopt-route-secret",
                "ANTHROPIC_AUTH_TOKEN": route_credential.expose_secret(),
                "UNRELATED": "preserved"
            }
        }))
        .unwrap();
        fs::write(
            user_home.join(".claude/settings.json"),
            &route_secret_configuration,
        )
        .unwrap();
        let route_secret_view = store.target_view_for(Target::Claude).await.unwrap();
        let route_endpoint = activation.model_endpoint_for(Target::Claude).await.unwrap();
        let mut route_stream = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut route_stream).await;
        let route_open = request(
            &mut route_stream,
            "route-open",
            open_operation(Target::Claude, &user_home),
        )
        .await;
        let route_preview = request(
            &mut route_stream,
            "route-preview",
            json!({"kind":"preview-reconciliation","target":"claude","strategy":"adopt"}),
        )
        .await;
        let route_rejection = request_raw(
            &mut route_stream,
            "route-apply",
            json!({
                "kind":"act","target":"claude","actionId":Uuid::new_v4(),
                "expectedRevision":route_open["result"]["view"]["managementRevision"],
                "action":{"kind":"reconcile","strategy":"adopt",
                    "observationToken":route_preview["result"]["preview"]["observationToken"],
                    "acknowledgeVersion":"77.1.0 (Claude Code)"}
            }),
        )
        .await;
        assert_raw_frame_is_safe(&route_rejection, &[route_credential.expose_secret()]);
        let route_rejection: Value = serde_json::from_slice(&route_rejection).unwrap();
        assert_eq!(
            route_rejection["problem"]["code"],
            "invalid-provider-credential"
        );
        assert!(
            fs::read(user_home.join(".claude/settings.json")).unwrap()
                == route_secret_configuration,
            "rejected Claude Adopt changed the Managed Configuration"
        );
        assert_eq!(
            store.target_view_for(Target::Claude).await.unwrap(),
            route_secret_view
        );
        assert_eq!(
            activation.model_endpoint_for(Target::Claude).await,
            Some(route_endpoint)
        );
        drop(route_stream);
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
        assert_eq!(view["mode"], "direct");
        assert_eq!(view["takeover"]["state"], "inactive");
        assert!(view["takeover"]["endpoint"].is_null());
        assert!(view["servingProviderId"].is_null());
        assert_eq!(view["activatedSnapshot"]["providerId"], current_id);
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
        let activated_after_adopt = request(
            &mut stream,
            "activate-after-adopt",
            json!({
                "kind":"act","target":"claude","actionId":Uuid::new_v4(),
                "expectedRevision":view["managementRevision"],
                "action":{"kind":"activate-provider","providerId":current_id,"mode":"takeover"}
            }),
        )
        .await;
        assert_eq!(
            activated_after_adopt["type"], "response",
            "Claude {suffix} Adopt could not re-enter Takeover: {:?}",
            activated_after_adopt["problem"]["code"]
        );
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
    let next_unknown_probe = fs::read_to_string(&claude_executable)
        .unwrap()
        .replace("77.1.0", "77.2.0");
    fs::write(&claude_executable, next_unknown_probe).unwrap();
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
    let compatibility_push = read_frame(&mut claude).await.unwrap();
    assert_eq!(compatibility_push["type"], "target-view");
    assert!(
        compatibility_push["view"]["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem["code"] == "compatibility-acknowledgement-required")
    );
    let compatibility_preview = request(
        &mut claude,
        "peer-compatibility-preview",
        json!({"kind":"preview-reconciliation","target":"claude","strategy":"reapply"}),
    )
    .await;
    assert_eq!(compatibility_preview["type"], "response");
    assert_eq!(
        compatibility_preview["result"]["preview"]["compatibility"]["version"],
        "77.2.0 (Claude Code)"
    );
    assert_eq!(
        compatibility_preview["result"]["preview"]["compatibility"]["acknowledgementRequired"],
        true
    );
    let compatibility_action_id = Uuid::new_v4();
    let compatibility_token = compatibility_preview["result"]["preview"]["observationToken"]
        .as_str()
        .unwrap();
    let reconciled = request(
        &mut claude,
        "peer-compatibility-apply",
        json!({
            "kind":"act","target":"claude","actionId":compatibility_action_id,
            "expectedRevision":claude_revision,
            "action":{
                "kind":"reconcile","strategy":"reapply","observationToken":compatibility_token,
                "acknowledgeVersion":"77.2.0 (Claude Code)"
            }
        }),
    )
    .await;
    assert_eq!(reconciled["type"], "response");
    assert!(
        !reconciled["result"]["outcome"]["view"]["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem["code"] == "compatibility-acknowledgement-required")
    );
    let reconciled_push = read_frame(&mut claude).await.unwrap();
    assert_eq!(reconciled_push["type"], "target-view");
    assert!(
        !reconciled_push["view"]["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem["code"] == "compatibility-acknowledgement-required")
    );
    assert!(
        !store
            .target_view_for(Target::Claude)
            .await
            .unwrap()
            .problems
            .iter()
            .any(|problem| problem.code == "compatibility-acknowledgement-required")
    );
    let replay = request(
        &mut claude,
        "peer-compatibility-replay",
        json!({
            "kind":"act","target":"claude","actionId":compatibility_action_id,
            "expectedRevision":0,"action":{"malformed":true}
        }),
    )
    .await;
    assert_eq!(replay["result"]["outcome"]["status"], "replayed");
    assert!(
        !replay["result"]["outcome"]["view"]["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem["code"] == "compatibility-acknowledgement-required")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), read_frame(&mut claude))
            .await
            .is_err()
    );
    let claude_revision = reconciled["result"]["outcome"]["view"]["managementRevision"]
        .as_u64()
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
    for (index, (request_id, action)) in [
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
    ]
    .into_iter()
    .enumerate()
    {
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
        if index == 0 {
            let incompatible_push = read_frame(&mut claude).await.unwrap();
            assert_eq!(incompatible_push["type"], "target-view");
        }
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
