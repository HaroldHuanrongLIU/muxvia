#![cfg(unix)]

use std::{fs, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use muxvia_routing::{
    claude::{ClaudeCapability, ClaudeProbe, ClaudeProblem},
    codex::{CodexCapability, CodexProbe, CodexProblem},
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
use tokio::net::UnixStream;
use uuid::Uuid;

struct TestedCodex;

impl CodexProbe for TestedCodex {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        Ok(CodexCapability::Tested {
            version: "test".into(),
        })
    }
}

struct TestedClaude;

impl ClaudeProbe for TestedClaude {
    fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
        Ok(ClaudeCapability::Tested {
            version: "test".into(),
        })
    }
}

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

struct DirectProvider<'a> {
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

async fn activate_direct(
    socket: &Path,
    target: Target,
    user_home: &Path,
    provider: DirectProvider<'_>,
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
                "mode": "direct"
            }
        }),
    )
    .await;
    assert_eq!(activated["type"], "response");
    let _push = read_frame(&mut stream).await.unwrap();
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
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(TestedCodex),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(Arc::new(TestedClaude), "/usr/bin/claude".into()),
    );
    let handle = ControlServer::bind_with_activation(&home, Arc::clone(&store), "test", activation)
        .await
        .unwrap();
    activate_direct(
        handle.socket_path(),
        Target::Codex,
        &user_home,
        DirectProvider {
            name: "Codex",
            base_url: "https://api.openai.test/v1",
            model: "gpt-test",
            authentication: "openai-bearer",
            secret: "codex-preview-secret",
        },
    )
    .await;
    activate_direct(
        handle.socket_path(),
        Target::Claude,
        &user_home,
        DirectProvider {
            name: "Claude",
            base_url: "https://api.anthropic.test",
            model: "claude-test",
            authentication: "anthropic-api-key",
            secret: "claude-preview-secret",
        },
    )
    .await;

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
            let response = request(
                &mut stream,
                strategy,
                json!({
                    "kind": "preview-reconciliation",
                    "target": target.as_str(),
                    "strategy": strategy
                }),
            )
            .await;
            assert_eq!(response["type"], "response");
            assert_eq!(response["result"]["kind"], "reconciliation-preview");
            assert_eq!(response["result"]["preview"]["target"], target.as_str());
            assert_eq!(response["result"]["preview"]["strategy"], strategy);
            if target == Target::Codex && strategy == "reapply" {
                first_reapply_token = response["result"]["preview"]["observationToken"]
                    .as_str()
                    .map(str::to_owned);
            }
            let wire = response.to_string();
            assert!(!wire.contains("codex-preview-secret"));
            assert!(!wire.contains("claude-preview-secret"));
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

    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
}
