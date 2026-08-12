#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use fs2::FileExt;
use muxvia_routing::control::framing::{read_frame, write_frame};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    net::UnixStream,
    process::{Child, Command},
    time::timeout,
};
use uuid::Uuid;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(8);

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
                    "kind": "save-provider", "name": "Committed before exit",
                    "baseUrl": "https://provider.example/v1", "model": "gpt-test",
                    "credential": "pending-secret-must-not-escape"
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
async fn test_only_options_are_hidden_and_reject_normal_invocation() {
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/muxvia-routing");
    let help = Command::new(&binary).arg("--help").output().await.unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(!help.contains("test-shutdown"));
    assert!(!help.contains("test-codex"));

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
