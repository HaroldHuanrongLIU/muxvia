#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use muxvia_routing::{
    control::{
        framing::{FrameError, read_frame, write_frame},
        server::{ControlServer, peer_uid_matches},
    },
    home::MuxviaHome,
    state::StateStore,
};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::UnixStream};
use uuid::Uuid;

struct ControlFixture {
    root: PathBuf,
    store: Arc<StateStore>,
    handle: Option<muxvia_routing::control::server::ControlServerHandle>,
}

impl ControlFixture {
    async fn start() -> Self {
        let root = short_temp_root("mx-ctl");
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let handle = ControlServer::bind(&home, Arc::clone(&store), "routing-test")
            .await
            .unwrap();
        Self {
            root,
            store,
            handle: Some(handle),
        }
    }

    fn socket(&self) -> &Path {
        self.handle.as_ref().unwrap().socket_path()
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(self.socket()).await.unwrap()
    }

    async fn shutdown(&mut self) {
        self.handle.take().unwrap().shutdown().await.unwrap();
    }
}

fn short_temp_root(prefix: &str) -> PathBuf {
    let id = Uuid::new_v4().simple().to_string();
    PathBuf::from("/tmp").join(format!("{prefix}-{}", &id[..8]))
}

impl Drop for ControlFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn hello(stream: &mut UnixStream) -> Value {
    write_frame(
        stream,
        &json!({
            "type": "hello",
            "rpc": { "major": 1, "minor": 0 },
            "release": "control-test"
        }),
    )
    .await
    .unwrap();
    read_frame(stream).await.unwrap()
}

async fn request(stream: &mut UnixStream, request_id: &str, operation: Value) -> Value {
    write_frame(
        stream,
        &json!({
            "type": "request",
            "requestId": request_id,
            "operation": operation,
        }),
    )
    .await
    .unwrap();
    read_frame(stream).await.unwrap()
}

fn save_action(name: &str, secret: &str) -> Value {
    json!({
        "kind": "save-provider",
        "name": name,
        "baseUrl": "https://provider.example/v1",
        "model": "model-test",
        "credential": secret,
    })
}

#[tokio::test]
async fn socket_and_runtime_are_private_and_shutdown_removes_socket() {
    let mut fixture = ControlFixture::start().await;
    let socket = fixture.socket().to_owned();
    let run = socket.parent().unwrap();

    assert!(
        fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert_eq!(
        fs::metadata(run).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );

    fixture.shutdown().await;
    assert!(!socket.exists());
}

#[tokio::test]
async fn bind_rejects_a_non_socket_or_symlink_collision() {
    for symlink in [false, true] {
        let root = short_temp_root("mx-col");
        let user_home = root.join("home");
        let run = user_home.join(".muxvia/run");
        fs::create_dir_all(&run).unwrap();
        let socket = run.join("control.sock");
        if symlink {
            std::os::unix::fs::symlink(root.join("missing"), &socket).unwrap();
        } else {
            fs::write(&socket, b"not a socket").unwrap();
        }
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());

        assert!(ControlServer::bind(&home, store, "test").await.is_err());
        assert!(fs::symlink_metadata(&socket).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn peer_uid_mismatch_is_rejected() {
    assert!(peer_uid_matches(501, 501));
    assert!(!peer_uid_matches(502, 501));
}

#[tokio::test]
async fn same_uid_connection_negotiates_without_opening_state() {
    let fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;

    let reply = hello(&mut stream).await;
    assert_eq!(reply["type"], "hello-ack");
    assert_eq!(reply["rpc"], json!({ "major": 1, "minor": 0 }));
    assert_eq!(reply["release"], "routing-test");
    assert_eq!(reply["frameLimit"], 1_048_576);
    assert_eq!(
        fixture
            .store
            .target_view()
            .await
            .unwrap()
            .management_revision,
        0
    );
}

#[tokio::test]
async fn major_mismatch_closes_before_opening_state() {
    let fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    write_frame(
        &mut stream,
        &json!({"type":"hello","rpc":{"major":2,"minor":0},"release":"test"}),
    )
    .await
    .unwrap();

    let reply = read_frame(&mut stream).await.unwrap();
    assert_eq!(reply["problem"]["code"], "protocol-mismatch");
    assert!(read_frame(&mut stream).await.is_err());
    assert_eq!(
        fixture
            .store
            .target_view()
            .await
            .unwrap()
            .management_revision,
        0
    );
}

#[tokio::test]
async fn requests_before_hello_and_second_hello_are_rejected() {
    let fixture = ControlFixture::start().await;
    let mut first = fixture.connect().await;
    let reply = request(
        &mut first,
        "before",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(reply["problem"]["code"], "handshake-required");

    let mut second = fixture.connect().await;
    hello(&mut second).await;
    write_frame(
        &mut second,
        &json!({"type":"hello","rpc":{"major":1,"minor":0},"release":"again"}),
    )
    .await
    .unwrap();
    let reply = read_frame(&mut second).await.unwrap();
    assert_eq!(reply["problem"]["code"], "unexpected-hello");
}

#[tokio::test]
async fn malformed_oversized_and_unknown_frames_execute_no_action() {
    let fixture = ControlFixture::start().await;

    let mut malformed = fixture.connect().await;
    malformed.write_all(&[0, 0, 0, 1, b'{']).await.unwrap();
    malformed.shutdown().await.unwrap();

    let mut oversized = fixture.connect().await;
    oversized
        .write_all(&(1_048_577_u32).to_be_bytes())
        .await
        .unwrap();
    oversized.shutdown().await.unwrap();

    let mut unknown = fixture.connect().await;
    hello(&mut unknown).await;
    let reply = request(
        &mut unknown,
        "unknown",
        json!({ "kind": "erase-everything", "credential": "do-not-echo" }),
    )
    .await;
    assert_eq!(reply["problem"]["code"], "unsupported-operation");
    assert!(!reply.to_string().contains("do-not-echo"));

    assert_eq!(
        fixture
            .store
            .target_view()
            .await
            .unwrap()
            .management_revision,
        0
    );
}

#[tokio::test]
async fn open_target_subscribes_and_action_responds_before_complete_push() {
    let fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(opened["result"]["kind"], "target-view");
    assert_eq!(opened["result"]["view"]["managementRevision"], 0);

    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "act",
            "operation": {
                "kind": "act", "target": "codex",
                "actionId": "00000000-0000-4000-8000-000000000003",
                "expectedRevision": 0,
                "action": save_action("Provider", "server-secret-must-not-escape")
            }
        }),
    )
    .await
    .unwrap();
    let response = read_frame(&mut stream).await.unwrap();
    let push = read_frame(&mut stream).await.unwrap();

    assert_eq!(response["type"], "response");
    assert_eq!(response["result"]["outcome"]["status"], "applied");
    assert_eq!(push["type"], "target-view");
    assert_eq!(push["view"], response["result"]["outcome"]["view"]);
    assert!(!format!("{response}{push}").contains("server-secret-must-not-escape"));
}

#[tokio::test]
async fn stale_revision_and_replayed_malformed_action_use_authoritative_boundary() {
    let fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let action_id = "00000000-0000-4000-8000-000000000004";
    let applied = request(
        &mut stream,
        "first",
        json!({
            "kind": "act", "target": "codex", "actionId": action_id,
            "expectedRevision": 0, "action": save_action("First", "first-secret")
        }),
    )
    .await;
    assert_eq!(applied["result"]["outcome"]["status"], "applied");
    let _push = read_frame(&mut stream).await.unwrap();

    let replay = request(
        &mut stream,
        "replay",
        json!({
            "kind": "act", "target": "codex", "actionId": action_id,
            "expectedRevision": 999, "action": { "kind": "save-provider", "name": 42 }
        }),
    )
    .await;
    assert_eq!(replay["result"]["outcome"]["status"], "replayed");
    let _push = read_frame(&mut stream).await.unwrap();

    let stale = request(
        &mut stream,
        "stale",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000005",
            "expectedRevision": 0, "action": save_action("Stale", "stale-secret")
        }),
    )
    .await;
    assert_eq!(stale["problem"]["code"], "stale-revision");
    assert_eq!(stale["authoritativeView"]["managementRevision"], 1);
    assert_eq!(
        fixture.store.target_view().await.unwrap().providers.len(),
        1
    );
}

#[tokio::test]
async fn stale_socket_is_replaced_only_when_it_is_a_socket() {
    let mut fixture = ControlFixture::start().await;
    let socket = fixture.socket().to_owned();
    fixture.shutdown().await;

    let stale = tokio::net::UnixListener::bind(&socket).unwrap();
    drop(stale);
    assert!(
        fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_socket()
    );

    let user_home = socket.parent().unwrap().parent().unwrap().parent().unwrap();
    let home = MuxviaHome::from_user_home(user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let handle = ControlServer::bind(&home, store, "test").await.unwrap();
    handle.shutdown().await.unwrap();
}

#[test]
fn frame_error_type_is_used_by_real_socket_helpers() {
    let error = FrameError::FrameTooLarge;
    assert_eq!(error.to_string(), "frame-too-large");
}
