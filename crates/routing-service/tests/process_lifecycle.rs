#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use axum::{Router, routing::post};
use fs2::FileExt;
use muxvia_routing::{
    claude::ClaudeConfigCodec,
    control::{
        framing::{read_frame, write_frame},
        protocol::Target,
    },
    home::MuxviaHome,
    state::{RecoveryIntent, RecoveryState, StateStore},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    net::{TcpListener, UnixStream},
    process::{Child, Command},
    time::timeout,
};
use uuid::Uuid;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(8);

fn assert_claude_direct_process_surface_is_secret_free(value: &Value) {
    let encoded = value.to_string();
    assert!(
        !encoded.contains("provider-secret")
            && !encoded.contains("ANTHROPIC_API_KEY")
            && !encoded.contains("ANTHROPIC_AUTH_TOKEN"),
        "Claude Direct process surface exposed a credential or setting"
    );
}

struct ProcessFixture {
    _root: TempDir,
    home: PathBuf,
    shutdown_file: PathBuf,
    child: Child,
}

impl ProcessFixture {
    async fn start() -> Self {
        let root = TempDir::new().unwrap();
        let user_home = root.path().join("home");
        let home = user_home.join(".muxvia");
        let shutdown_file = root.path().join("shutdown");
        fs::create_dir_all(&user_home).unwrap();
        let child = command(&home, &shutdown_file).spawn().unwrap();
        wait_for_socket(&home.join("run/control.sock")).await;
        Self {
            _root: root,
            home,
            shutdown_file,
            child,
        }
    }

    fn socket(&self) -> PathBuf {
        self.home.join("run/control.sock")
    }

    fn database(&self) -> PathBuf {
        self.home.join("state/muxvia.db")
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(self.socket()).await.unwrap()
    }

    async fn shutdown(mut self) {
        fs::write(&self.shutdown_file, b"shutdown\n").unwrap();
        let status = timeout(PROCESS_TIMEOUT, self.child.wait())
            .await
            .expect("service did not stop")
            .unwrap();
        assert!(status.success());
        assert!(!self.socket().exists());
    }
}

impl Drop for ProcessFixture {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn command(home: &Path, shutdown_file: &Path) -> Command {
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/muxvia-routing");
    let mut command = Command::new(binary);
    command
        .arg("--home")
        .arg(home)
        .arg("--test-shutdown-file")
        .arg(shutdown_file)
        .env("MUXVIA_INTEGRATION_TEST", "1")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn fake_cli(root: &Path, name: &str, version: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) printf '%s\\n' '{version}' ;;\n  --help) printf '%s\\n' 'Usage: {name} --config --settings --model' ;;\n  *) exit 64 ;;\nesac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

async fn wait_for_socket(socket: &Path) {
    timeout(PROCESS_TIMEOUT, async {
        loop {
            if fs::symlink_metadata(socket).is_ok_and(|metadata| {
                metadata.file_type().is_socket() && metadata.permissions().mode() & 0o777 == 0o600
            }) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("control socket did not become ready");
}

async fn hello(stream: &mut UnixStream) {
    write_frame(
        stream,
        &json!({
            "type": "hello",
            "rpc": { "major": 1, "minor": 0 },
            "release": "process-test"
        }),
    )
    .await
    .unwrap();
    assert_eq!(read_frame(stream).await.unwrap()["type"], "hello-ack");
}

async fn request(stream: &mut UnixStream, request_id: &str, operation: Value) -> Value {
    write_frame(
        stream,
        &json!({ "type": "request", "requestId": request_id, "operation": operation }),
    )
    .await
    .unwrap();
    read_frame(stream).await.unwrap()
}

fn fingerprint(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap()
}

#[tokio::test]
async fn second_service_exits_before_opening_the_database() {
    let fixture = ProcessFixture::start().await;
    let before = fingerprint(&fixture.database());
    let output = command(&fixture.home, &fixture.shutdown_file)
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(73));
    assert_eq!(fingerprint(&fixture.database()), before);
    assert!(!fixture.database().with_extension("db-wal").exists());
    assert!(!fixture.database().with_extension("db-shm").exists());
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_preheld_lock_prevents_any_database_open_or_migration() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home/.muxvia");
    fs::create_dir_all(&home).unwrap();
    let lock_path = home.join("service.lock");
    let lock = fs::File::create(&lock_path).unwrap();
    lock.try_lock_exclusive().unwrap();
    let database = home.join("state/muxvia.db");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    fs::write(&database, b"not-sqlite-canary").unwrap();
    let before = fingerprint(&database);

    let output = command(&home, &root.path().join("shutdown"))
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(73));
    assert_eq!(fingerprint(&database), before);
    assert!(!database.with_extension("db-wal").exists());
    assert!(!database.with_extension("db-shm").exists());
}

#[tokio::test]
async fn creates_private_runtime_state_lock_and_database() {
    let fixture = ProcessFixture::start().await;
    for directory in [
        fixture.home.clone(),
        fixture.home.join("run"),
        fixture.home.join("state"),
    ] {
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for file in [fixture.home.join("service.lock"), fixture.database()] {
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(
        fs::metadata(fixture.socket()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn replaces_only_a_stale_socket_before_accepting_the_first_bounded_connection() {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home/.muxvia");
    let run = home.join("run");
    fs::create_dir_all(&run).unwrap();
    let socket = run.join("control.sock");
    let stale = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    drop(stale);
    let shutdown_file = root.path().join("shutdown");
    let mut child = command(&home, &shutdown_file).spawn().unwrap();
    wait_for_socket(&socket).await;
    assert!(
        timeout(Duration::from_millis(100), child.wait())
            .await
            .is_err(),
        "a fresh inactive service must wait for the Control Plane's bounded first attempt"
    );

    let mut stream = UnixStream::connect(&socket).await.unwrap();
    hello(&mut stream).await;
    request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    fs::write(&shutdown_file, b"shutdown\n").unwrap();
    assert!(
        timeout(PROCESS_TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn inactive_service_exits_after_its_last_accepted_session() {
    let mut fixture = ProcessFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    drop(stream);

    let status = timeout(PROCESS_TIMEOUT, fixture.child.wait())
        .await
        .expect("inactive service remained after its last session")
        .unwrap();
    assert!(status.success());
    assert!(!fixture.socket().exists());
}

#[tokio::test]
async fn clean_claude_direct_is_control_only_across_restart_and_exits_after_last_session() {
    let root = TempDir::new().unwrap();
    let user_home = root.path().join("home");
    let home = user_home.join(".muxvia");
    fs::create_dir_all(&user_home).unwrap();
    let claude = fake_cli(root.path(), "fake-claude-direct", "2.1.37 (Claude Code)");

    let first_shutdown = root.path().join("first-shutdown");
    let mut first = command(&home, &first_shutdown)
        .arg("--test-claude-executable")
        .arg(&claude)
        .spawn()
        .unwrap();
    let socket = home.join("run/control.sock");
    wait_for_socket(&socket).await;
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open-claude",
        json!({
            "kind": "open-target", "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null, "selectorState": "unset",
                "hostManagedState": "unmanaged", "cwd": user_home
            }
        }),
    )
    .await;
    assert_claude_direct_process_surface_is_secret_free(&opened);
    let saved = request(
        &mut stream,
        "save-claude",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider", "name": "Claude Direct",
                "baseUrl": "https://api.anthropic.test", "model": "claude-direct",
                "credential": {"kind": "replace", "value": "provider-secret"},
                "authentication": "anthropic-api-key", "presetKey": null
            }
        }),
    )
    .await;
    assert_claude_direct_process_surface_is_secret_free(&saved);
    let save_push = read_frame(&mut stream).await.unwrap();
    assert_claude_direct_process_surface_is_secret_free(&save_push);
    let provider_id = saved["result"]["outcome"]["view"]["providers"][0]["id"].clone();
    let applied = request(
        &mut stream,
        "activate-direct",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": 1,
            "action": {"kind": "activate-provider", "providerId": provider_id, "mode": "direct"}
        }),
    )
    .await;
    assert_claude_direct_process_surface_is_secret_free(&applied);
    let applied_view = &applied["result"]["outcome"]["view"];
    assert_eq!(applied_view["mode"], "direct");
    assert!(applied_view["takeover"]["endpoint"].is_null());
    let snapshot_id = applied_view["activatedSnapshot"]["id"].clone();
    let activation_push = read_frame(&mut stream).await.unwrap();
    assert_claude_direct_process_surface_is_secret_free(&activation_push);
    drop(stream);
    assert!(
        timeout(PROCESS_TIMEOUT, first.wait())
            .await
            .expect("Claude Direct retained the first service epoch")
            .unwrap()
            .success()
    );
    assert!(!socket.exists());

    let second_shutdown = root.path().join("second-shutdown");
    let mut second = command(&home, &second_shutdown)
        .arg("--test-claude-executable")
        .arg(&claude)
        .spawn()
        .unwrap();
    wait_for_socket(&socket).await;
    let mut reopened = UnixStream::connect(&socket).await.unwrap();
    hello(&mut reopened).await;
    let reopened_view = request(
        &mut reopened,
        "reopen-claude",
        json!({"kind": "open-target", "target": "claude"}),
    )
    .await;
    assert_claude_direct_process_surface_is_secret_free(&reopened_view);
    assert_eq!(reopened_view["result"]["view"]["mode"], "direct");
    assert_eq!(
        reopened_view["result"]["view"]["activatedSnapshot"]["id"],
        snapshot_id
    );
    assert!(reopened_view["result"]["view"]["takeover"]["endpoint"].is_null());
    drop(reopened);
    assert!(
        timeout(PROCESS_TIMEOUT, second.wait())
            .await
            .expect("Claude Direct retained the restarted service epoch")
            .unwrap()
            .success()
    );
    assert!(!socket.exists());
}

#[tokio::test]
async fn unresolved_claude_recovery_or_drift_keeps_control_only_process_alive() {
    for state in ["recovery-required", "configuration-drift"] {
        let root = TempDir::new().unwrap();
        let user_home = root.path().join("home");
        let home_path = user_home.join(".muxvia");
        let shutdown_file = root.path().join(format!("shutdown-{state}"));
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = StateStore::open(&home).await.unwrap();
        if state == "recovery-required" {
            let codec = ClaudeConfigCodec::for_user_home(&user_home).unwrap();
            let intent = RecoveryIntent::pending_claude(
                Uuid::new_v4(),
                Uuid::new_v4(),
                codec.settings_path().to_owned(),
                codec.inspect().unwrap(),
                codec.desired_takeover("claude-test", "http://127.0.0.1:43124", "routing-secret"),
                0,
            );
            store.insert_recovery_intent(&intent).await.unwrap();
            store
                .set_recovery_state(intent.id(), RecoveryState::RecoveryRequired)
                .await
                .unwrap();
        } else {
            store
                .mark_configuration_drift_for(Target::Claude)
                .await
                .unwrap();
        }
        drop(store);

        let mut child = command(&home_path, &shutdown_file).spawn().unwrap();
        let socket = home_path.join("run/control.sock");
        wait_for_socket(&socket).await;
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        hello(&mut stream).await;
        request(
            &mut stream,
            "open",
            json!({"kind":"open-target","target":"claude"}),
        )
        .await;
        drop(stream);
        assert!(
            timeout(Duration::from_millis(200), child.wait())
                .await
                .is_err(),
            "unresolved {state} did not retain control-only service lifetime"
        );
        fs::write(&shutdown_file, b"shutdown\n").unwrap();
        assert!(
            timeout(PROCESS_TIMEOUT, child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

#[tokio::test]
async fn disconnected_pending_action_commits_before_inactive_exit() {
    let mut fixture = ProcessFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "save",
            "operation": {
                "kind": "act", "target": "codex",
                "actionId": Uuid::new_v4(), "expectedRevision": 0,
                "action": {
                    "kind": "create-provider", "name": "Committed before exit",
                    "baseUrl": "https://provider.example/v1", "model": "gpt-test",
                    "credential": { "kind": "replace", "value": "pending-secret-must-not-escape" },
                    "presetKey": null
                }
            }
        }),
    )
    .await
    .unwrap();
    drop(stream);

    assert!(
        timeout(PROCESS_TIMEOUT, fixture.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let database = tokio_rusqlite::Connection::open(fixture.database())
        .await
        .unwrap();
    let provider_count = database
        .call(|connection| {
            connection.query_row("SELECT COUNT(*) FROM providers", [], |row| {
                row.get::<_, u64>(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(provider_count, 1);
}

#[tokio::test]
async fn explicit_test_shutdown_drains_sessions_and_removes_its_socket() {
    let mut fixture = ProcessFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    let mut shutdown = fs::File::create(&fixture.shutdown_file).unwrap();
    shutdown.write_all(b"shutdown\n").unwrap();
    shutdown.sync_all().unwrap();
    assert!(
        timeout(PROCESS_TIMEOUT, fixture.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!fixture.socket().exists());
    assert!(read_frame(&mut stream).await.is_err());
}

#[tokio::test]
async fn either_takeover_keeps_the_process_alive_and_shutdown_drains_both_routes_and_sessions() {
    let root = TempDir::new().unwrap();
    let user_home = root.path().join("home");
    let home = user_home.join(".muxvia");
    let shutdown_file = root.path().join("shutdown");
    fs::create_dir_all(&user_home).unwrap();
    let codex = fake_cli(root.path(), "fake-codex", "codex-cli 0.106.0");
    let claude = fake_cli(root.path(), "fake-claude", "2.1.37 (Claude Code)");
    let mut child = command(&home, &shutdown_file)
        .arg("--test-codex-executable")
        .arg(codex)
        .arg("--test-claude-executable")
        .arg(claude)
        .spawn()
        .unwrap();
    let socket = home.join("run/control.sock");
    wait_for_socket(&socket).await;
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    hello(&mut stream).await;
    request(
        &mut stream,
        "open-claude",
        json!({
            "kind": "open-target", "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null, "selectorState": "unset",
                "hostManagedState": "unmanaged", "cwd": user_home
            }
        }),
    )
    .await;
    let saved = request(
        &mut stream,
        "save-claude",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider", "name": "Claude",
                "baseUrl": "https://api.anthropic.test", "model": "claude-test",
                "credential": {"kind": "replace", "value": "provider-secret"},
                "authentication": "anthropic-api-key", "presetKey": null
            }
        }),
    )
    .await;
    read_frame(&mut stream).await.unwrap();
    let provider_id = saved["result"]["outcome"]["view"]["providers"][0]["id"].clone();
    let applied = request(
        &mut stream,
        "activate-claude",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": 1,
            "action": {"kind": "activate-provider", "providerId": provider_id, "mode": "takeover"}
        }),
    )
    .await;
    read_frame(&mut stream).await.unwrap();
    let endpoint: std::net::SocketAddr =
        applied["result"]["outcome"]["view"]["takeover"]["endpoint"]
            .as_str()
            .unwrap()
            .trim_start_matches("http://")
            .parse()
            .unwrap();
    drop(stream);

    let mut codex_stream = UnixStream::connect(&socket).await.unwrap();
    hello(&mut codex_stream).await;
    request(
        &mut codex_stream,
        "open-codex",
        json!({"kind": "open-target", "target": "codex"}),
    )
    .await;
    let codex_saved = request(
        &mut codex_stream,
        "save-codex",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider", "name": "Codex",
                "baseUrl": "https://api.openai.test/v1", "model": "gpt-test",
                "credential": {"kind": "replace", "value": "codex-provider-secret"},
                "presetKey": null
            }
        }),
    )
    .await;
    read_frame(&mut codex_stream).await.unwrap();
    let codex_provider = codex_saved["result"]["outcome"]["view"]["providers"][0]["id"].clone();
    let codex_applied = request(
        &mut codex_stream,
        "activate-codex",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 1,
            "action": {"kind": "activate-provider", "providerId": codex_provider, "mode": "takeover"}
        }),
    )
    .await;
    read_frame(&mut codex_stream).await.unwrap();
    let codex_endpoint: std::net::SocketAddr =
        codex_applied["result"]["outcome"]["view"]["takeover"]["endpoint"]
            .as_str()
            .unwrap()
            .trim_start_matches("http://")
            .parse()
            .unwrap();
    assert_ne!(endpoint, codex_endpoint);

    assert!(
        timeout(Duration::from_millis(200), child.wait())
            .await
            .is_err(),
        "a committed Claude Takeover did not retain the Routing Service"
    );
    assert!(tokio::net::TcpStream::connect(endpoint).await.is_ok());
    assert!(tokio::net::TcpStream::connect(codex_endpoint).await.is_ok());
    fs::write(&shutdown_file, b"shutdown\n").unwrap();
    assert!(
        timeout(PROCESS_TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!socket.exists());
    assert!(read_frame(&mut codex_stream).await.is_err());
    assert!(tokio::net::TcpStream::connect(endpoint).await.is_err());
    assert!(
        tokio::net::TcpStream::connect(codex_endpoint)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn clean_dual_takeover_restart_resumes_exact_claude_route_snapshot_and_credential() {
    let root = TempDir::new().unwrap();
    let user_home = root.path().join("home");
    let home = user_home.join(".muxvia");
    let first_shutdown = root.path().join("shutdown-first");
    let second_shutdown = root.path().join("shutdown-second");
    fs::create_dir_all(&user_home).unwrap();
    let codex = fake_cli(root.path(), "fake-codex", "codex-cli 0.106.0");
    let claude = fake_cli(root.path(), "fake-claude", "2.1.37 (Claude Code)");
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_base = format!("http://{}/v1", upstream_listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        axum::serve(
            upstream_listener,
            Router::new().route("/v1/messages", post(|| async { "claude-restarted" })),
        )
        .await
        .unwrap();
    });
    let mut first = command(&home, &first_shutdown)
        .arg("--test-codex-executable")
        .arg(&codex)
        .arg("--test-claude-executable")
        .arg(&claude)
        .spawn()
        .unwrap();
    let socket = home.join("run/control.sock");
    wait_for_socket(&socket).await;

    let mut claude_stream = UnixStream::connect(&socket).await.unwrap();
    hello(&mut claude_stream).await;
    request(
        &mut claude_stream,
        "open-claude",
        json!({
            "kind": "open-target", "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null, "selectorState": "unset",
                "hostManagedState": "unmanaged", "cwd": user_home
            }
        }),
    )
    .await;
    let claude_saved = request(
        &mut claude_stream,
        "save-claude",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider", "name": "Claude",
                "baseUrl": upstream_base, "model": "claude-restart-model",
                "credential": {"kind": "replace", "value": "provider-secret"},
                "authentication": "anthropic-api-key", "presetKey": null
            }
        }),
    )
    .await;
    read_frame(&mut claude_stream).await.unwrap();
    let claude_provider = claude_saved["result"]["outcome"]["view"]["providers"][0]["id"].clone();
    let claude_applied = request(
        &mut claude_stream,
        "activate-claude",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": 1,
            "action": {
                "kind": "activate-provider", "providerId": claude_provider,
                "mode": "takeover"
            }
        }),
    )
    .await;
    read_frame(&mut claude_stream).await.unwrap();
    let claude_view = &claude_applied["result"]["outcome"]["view"];
    let claude_endpoint = claude_view["takeover"]["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    let claude_snapshot = claude_view["activatedSnapshot"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut codex_stream = UnixStream::connect(&socket).await.unwrap();
    hello(&mut codex_stream).await;
    request(
        &mut codex_stream,
        "open-codex",
        json!({"kind": "open-target", "target": "codex"}),
    )
    .await;
    let codex_saved = request(
        &mut codex_stream,
        "save-codex",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider", "name": "Codex",
                "baseUrl": "https://api.openai.test/v1", "model": "gpt-peer",
                "credential": {"kind": "replace", "value": "codex-provider-secret"},
                "presetKey": null
            }
        }),
    )
    .await;
    read_frame(&mut codex_stream).await.unwrap();
    let codex_provider = codex_saved["result"]["outcome"]["view"]["providers"][0]["id"].clone();
    let codex_applied = request(
        &mut codex_stream,
        "activate-codex",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 1,
            "action": {
                "kind": "activate-provider", "providerId": codex_provider,
                "mode": "takeover"
            }
        }),
    )
    .await;
    read_frame(&mut codex_stream).await.unwrap();
    let codex_view = &codex_applied["result"]["outcome"]["view"];
    let codex_endpoint = codex_view["takeover"]["endpoint"]
        .as_str()
        .unwrap()
        .to_owned();
    let codex_snapshot = codex_view["activatedSnapshot"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(claude_endpoint, codex_endpoint);
    let settings_path = user_home.join(".claude/settings.json");
    let settings_before = fs::read(&settings_path).unwrap();
    let settings: Value = serde_json::from_slice(&settings_before).unwrap();
    let claude_credential = settings["env"]["ANTHROPIC_AUTH_TOKEN"]
        .as_str()
        .unwrap()
        .to_owned();

    drop(claude_stream);
    drop(codex_stream);
    fs::write(&first_shutdown, b"shutdown\n").unwrap();
    assert!(
        timeout(PROCESS_TIMEOUT, first.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!socket.exists());

    let mut second = command(&home, &second_shutdown)
        .arg("--test-codex-executable")
        .arg(&codex)
        .arg("--test-claude-executable")
        .arg(&claude)
        .spawn()
        .unwrap();
    wait_for_socket(&socket).await;
    assert!(
        fs::read(&settings_path).unwrap() == settings_before,
        "clean restart changed Claude managed settings or Routing Credential"
    );

    let mut reopened_claude = UnixStream::connect(&socket).await.unwrap();
    hello(&mut reopened_claude).await;
    let reopened_claude_view = request(
        &mut reopened_claude,
        "reopen-claude",
        json!({"kind": "open-target", "target": "claude"}),
    )
    .await;
    let reopened_claude_view = &reopened_claude_view["result"]["view"];
    assert_eq!(
        reopened_claude_view["takeover"]["endpoint"],
        claude_endpoint
    );
    assert_eq!(
        reopened_claude_view["activatedSnapshot"]["id"],
        claude_snapshot
    );

    let mut reopened_codex = UnixStream::connect(&socket).await.unwrap();
    hello(&mut reopened_codex).await;
    let reopened_codex_view = request(
        &mut reopened_codex,
        "reopen-codex",
        json!({"kind": "open-target", "target": "codex"}),
    )
    .await;
    let reopened_codex_view = &reopened_codex_view["result"]["view"];
    assert_eq!(reopened_codex_view["takeover"]["endpoint"], codex_endpoint);
    assert_eq!(
        reopened_codex_view["activatedSnapshot"]["id"],
        codex_snapshot
    );
    assert_ne!(
        reopened_claude_view["takeover"]["endpoint"],
        reopened_codex_view["takeover"]["endpoint"]
    );

    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{claude_endpoint}/v1/messages"))
        .header("authorization", format!("Bearer {claude_credential}"))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(
            serde_json::to_vec(&json!({
                "model": "caller-model", "max_tokens": 1,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "claude-restarted");

    fs::write(&second_shutdown, b"shutdown\n").unwrap();
    assert!(
        timeout(PROCESS_TIMEOUT, second.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!socket.exists());
    upstream.abort();
}

#[tokio::test]
async fn test_only_options_are_hidden_and_reject_normal_invocation() {
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/muxvia-routing");
    let help = Command::new(&binary).arg("--help").output().await.unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(!help.contains("test-shutdown"));
    assert!(!help.contains("test-codex"));
    assert!(!help.contains("test-claude"));

    let root = TempDir::new().unwrap();
    let output = Command::new(binary)
        .arg("--home")
        .arg(root.path().join(".muxvia"))
        .arg("--test-shutdown-file")
        .arg(root.path().join("shutdown"))
        .env_remove("MUXVIA_INTEGRATION_TEST")
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
    assert!(!root.path().join(".muxvia").exists());
}

#[tokio::test]
async fn omitted_home_uses_only_the_effective_home_muxvia_root() {
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/muxvia-routing");
    let root = TempDir::new().unwrap();
    let user_home = root.path().join("home");
    fs::create_dir_all(&user_home).unwrap();
    let shutdown_file = root.path().join("shutdown");
    let mut child = Command::new(binary)
        .arg("--test-shutdown-file")
        .arg(&shutdown_file)
        .env("HOME", &user_home)
        .env("MUXVIA_INTEGRATION_TEST", "1")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let socket = user_home.join(".muxvia/run/control.sock");
    wait_for_socket(&socket).await;
    assert!(user_home.join(".muxvia/state/muxvia.db").exists());

    fs::write(&shutdown_file, b"shutdown\n").unwrap();
    assert!(
        timeout(PROCESS_TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!socket.exists());
}
