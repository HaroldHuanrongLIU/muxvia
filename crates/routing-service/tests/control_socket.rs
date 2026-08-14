#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixStream},
    sync::mpsc,
    task::JoinHandle,
};
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

struct HeldInspectionServer {
    base_url: String,
    started: mpsc::UnboundedReceiver<()>,
    dropped: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl HeldInspectionServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (started_tx, started) = mpsc::unbounded_channel();
        let (dropped_tx, dropped) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break };
                        let started_tx = started_tx.clone();
                        let dropped_tx = dropped_tx.clone();
                        connections.spawn(async move {
                            let mut request = Vec::new();
                            let mut chunk = [0_u8; 1024];
                            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                                let Ok(read) = stream.read(&mut chunk).await else { return };
                                if read == 0 { return; }
                                request.extend_from_slice(&chunk[..read]);
                            }
                            let _ = started_tx.send(());
                            loop {
                                match stream.read(&mut chunk).await {
                                    Ok(0) | Err(_) => {
                                        let _ = dropped_tx.send(());
                                        return;
                                    }
                                    Ok(_) => {}
                                }
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            base_url,
            started,
            dropped,
            task,
        }
    }

    async fn wait_started(&mut self) {
        tokio::time::timeout(Duration::from_secs(1), self.started.recv())
            .await
            .unwrap()
            .unwrap();
    }

    async fn wait_dropped(&mut self) {
        tokio::time::timeout(Duration::from_secs(1), self.dropped.recv())
            .await
            .unwrap()
            .unwrap();
    }
}

impl Drop for HeldInspectionServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct CompletedInspectionServer {
    base_url: String,
    task: JoinHandle<()>,
}

impl CompletedInspectionServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).await.unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let body = br#"{"data":[{"id":"model-complete"}]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
        });
        Self { base_url, task }
    }
}

impl Drop for CompletedInspectionServer {
    fn drop(&mut self) {
        self.task.abort();
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

async fn wait_for_zero_inspections(fixture: &ControlFixture) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if fixture.handle.as_ref().unwrap().tracked_inspections() == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn create_action(name: &str, secret: &str) -> Value {
    json!({
        "kind": "create-provider",
        "name": name,
        "baseUrl": "https://provider.example/v1",
        "model": "model-test",
        "credential": { "kind": "replace", "value": secret },
        "presetKey": null,
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
async fn shutdown_closes_accepted_sessions_before_returning() {
    let mut fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    assert_eq!(hello(&mut stream).await["type"], "hello-ack");

    fixture.shutdown().await;
    let _ = write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "after-shutdown",
            "operation": {
                "kind": "act", "target": "codex",
                "actionId": "00000000-0000-4000-8000-000000000006",
                "expectedRevision": 0,
                "action": create_action("Too late", "must-not-be-stored")
            }
        }),
    )
    .await;

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
async fn malformed_pre_hello_frames_return_a_generic_error_then_close() {
    let fixture = ControlFixture::start().await;

    let invalid_json = br#"{"credential":"do-not-echo",}"#;
    let cases = [
        vec![0, 0],
        vec![0, 0, 0, 4, b'{', b'}'],
        (1_048_577_u32).to_be_bytes().to_vec(),
        vec![0, 0, 0, 1, 0xff],
        [
            (invalid_json.len() as u32).to_be_bytes().as_slice(),
            invalid_json.as_slice(),
        ]
        .concat(),
    ];

    for bytes in cases {
        let mut stream = fixture.connect().await;
        stream.write_all(&bytes).await.unwrap();
        stream.shutdown().await.unwrap();
        let reply = read_frame(&mut stream).await.unwrap();
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["requestId"], Value::Null);
        assert_eq!(reply["problem"]["code"], "frame-invalid");
        assert!(!reply.to_string().contains("do-not-echo"));
        assert!(read_frame(&mut stream).await.is_err());
    }

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
async fn malformed_post_handshake_frame_returns_a_generic_error_and_executes_no_action() {
    let fixture = ControlFixture::start().await;
    let invalid_json = br#"{"credential":"do-not-echo",}"#;
    let cases = [
        vec![0, 0],
        vec![0, 0, 0, 4, b'{', b'}'],
        (1_048_577_u32).to_be_bytes().to_vec(),
        vec![0, 0, 0, 1, 0xff],
        [
            (invalid_json.len() as u32).to_be_bytes().as_slice(),
            invalid_json.as_slice(),
        ]
        .concat(),
    ];

    for bytes in cases {
        let mut malformed = fixture.connect().await;
        hello(&mut malformed).await;
        malformed.write_all(&bytes).await.unwrap();
        malformed.shutdown().await.unwrap();

        let reply = read_frame(&mut malformed).await.unwrap();
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["requestId"], Value::Null);
        assert_eq!(reply["problem"]["code"], "frame-invalid");
        assert!(!reply.to_string().contains("do-not-echo"));
        assert!(read_frame(&mut malformed).await.is_err());
    }

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
async fn unknown_frames_execute_no_action_without_echoing_input() {
    let fixture = ControlFixture::start().await;

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
                "action": create_action("Provider", "server-secret-must-not-escape")
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
async fn discovery_is_concurrent_cancellable_and_shutdown_drains_session_work() {
    let mut upstream = HeldInspectionServer::start().await;
    let mut fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let created = request(
        &mut stream,
        "create-inspected",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000081",
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider",
                "name": "Inspected",
                "baseUrl": upstream.base_url,
                "model": "model-test",
                "credential": { "kind": "replace", "value": "uds-secret-must-not-escape" },
                "presetKey": null
            }
        }),
    )
    .await;
    let provider = created["result"]["outcome"]["view"]["providers"][0].clone();

    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "discover-held",
            "operation": {
                "kind": "discover-models", "target": "codex",
                "source": {
                    "kind": "saved",
                    "providerId": provider["id"],
                    "providerRevision": provider["providerRevision"]
                }
            }
        }),
    )
    .await
    .unwrap();
    upstream.wait_started().await;

    write_frame(
        &mut stream,
        &json!({
            "type": "request", "requestId": "open-while-held",
            "operation": { "kind": "open-target", "target": "codex" }
        }),
    )
    .await
    .unwrap();
    let opened = tokio::time::timeout(Duration::from_millis(200), read_frame(&mut stream))
        .await
        .expect("open-target was blocked behind discovery")
        .unwrap();
    assert_eq!(opened["requestId"], "open-while-held");
    assert_eq!(opened["result"]["kind"], "target-view");
    let queued_push = tokio::time::timeout(Duration::from_millis(200), read_frame(&mut stream))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queued_push["type"], "target-view");
    assert_eq!(fixture.handle.as_ref().unwrap().tracked_inspections(), 1,);

    write_frame(
        &mut stream,
        &json!({ "type": "cancel", "requestId": "discover-held" }),
    )
    .await
    .unwrap();
    upstream.wait_dropped().await;
    wait_for_zero_inspections(&fixture).await;
    let late = tokio::time::timeout(Duration::from_millis(80), read_frame(&mut stream)).await;
    assert!(
        late.is_err(),
        "cancelled discovery wrote a result: {late:?}"
    );

    let usable = request(
        &mut stream,
        "open-after-cancel",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(usable["result"]["kind"], "target-view");

    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "discover-shutdown",
            "operation": {
                "kind": "discover-models", "target": "codex",
                "source": {
                    "kind": "saved",
                    "providerId": provider["id"],
                    "providerRevision": provider["providerRevision"]
                }
            }
        }),
    )
    .await
    .unwrap();
    upstream.wait_started().await;
    fixture.handle.as_mut().unwrap().request_shutdown();
    upstream.wait_dropped().await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn completed_and_disconnected_inspections_are_reaped_without_orphans() {
    let completed_upstream = CompletedInspectionServer::start().await;
    let mut fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let created = request(
        &mut stream,
        "create-completed",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000091",
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider", "name": "Completed",
                "baseUrl": completed_upstream.base_url, "model": "model-test",
                "credential": { "kind": "replace", "value": "completed-secret" },
                "presetKey": null
            }
        }),
    )
    .await;
    let provider = created["result"]["outcome"]["view"]["providers"][0].clone();
    let completed = request(
        &mut stream,
        "discover-completed",
        json!({
            "kind": "discover-models", "target": "codex",
            "source": {
                "kind": "saved", "providerId": provider["id"],
                "providerRevision": provider["providerRevision"]
            }
        }),
    )
    .await;
    assert_eq!(completed["result"]["kind"], "model-discovery");
    assert_eq!(completed["result"]["result"]["status"], "success");
    wait_for_zero_inspections(&fixture).await;

    let mut held_upstream = HeldInspectionServer::start().await;
    let updated = request(
        &mut stream,
        "move-to-held",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000092",
            "expectedRevision": 1,
            "action": {
                "kind": "update-provider", "providerId": provider["id"],
                "providerRevision": provider["providerRevision"], "name": "Held",
                "baseUrl": held_upstream.base_url, "model": "model-test",
                "credential": { "kind": "keep" }
            }
        }),
    )
    .await;
    let updated_provider = updated["result"]["outcome"]["view"]["providers"][0].clone();
    write_frame(
        &mut stream,
        &json!({
            "type": "request", "requestId": "discover-disconnect",
            "operation": {
                "kind": "discover-models", "target": "codex",
                "source": {
                    "kind": "saved", "providerId": updated_provider["id"],
                    "providerRevision": updated_provider["providerRevision"]
                }
            }
        }),
    )
    .await
    .unwrap();
    held_upstream.wait_started().await;
    drop(stream);
    held_upstream.wait_dropped().await;
    wait_for_zero_inspections(&fixture).await;
    fixture.shutdown().await;
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
            "expectedRevision": 0, "action": create_action("First", "first-secret")
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
            "expectedRevision": 999, "action": { "kind": "create-provider", "name": 42 }
        }),
    )
    .await;
    assert_eq!(replay["result"]["outcome"]["status"], "replayed");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            read_frame(&mut stream),
        )
        .await
        .is_err(),
        "a replay must not publish a duplicate Target View",
    );

    let stale = request(
        &mut stream,
        "stale",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000005",
            "expectedRevision": 0, "action": create_action("Stale", "stale-secret")
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
async fn reorder_and_delete_actions_are_receipt_first_and_publish_once_per_applied_mutation() {
    let fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    let mut provider_ids = Vec::new();
    for (request_id, action_id, name, secret, revision) in [
        (
            "create-one",
            "00000000-0000-4000-8000-000000000021",
            "One",
            "one-secret",
            0,
        ),
        (
            "create-two",
            "00000000-0000-4000-8000-000000000022",
            "Two",
            "two-secret",
            1,
        ),
    ] {
        let response = request(
            &mut stream,
            request_id,
            json!({
                "kind": "act", "target": "codex", "actionId": action_id,
                "expectedRevision": revision, "action": create_action(name, secret),
            }),
        )
        .await;
        provider_ids.push(
            response["result"]["outcome"]["view"]["providers"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["id"]
                .clone(),
        );
        let push = read_frame(&mut stream).await.unwrap();
        assert_eq!(response["result"]["outcome"]["status"], "applied");
        assert_eq!(push["type"], "target-view");
        assert_eq!(push["view"], response["result"]["outcome"]["view"]);
    }

    let reordered = request(
        &mut stream,
        "reorder",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000023",
            "expectedRevision": 2,
            "action": {
                "kind": "reorder-providers",
                "providerIds": [provider_ids[1].clone(), provider_ids[0].clone()]
            }
        }),
    )
    .await;
    assert_eq!(reordered["result"]["outcome"]["status"], "applied");
    let push = read_frame(&mut stream).await.unwrap();
    assert_eq!(push["view"], reordered["result"]["outcome"]["view"]);

    let provider = reordered["result"]["outcome"]["view"]["providers"][0].clone();
    let replay = request(
        &mut stream,
        "replay",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000023",
            "expectedRevision": 999,
            "action": { "malformed": "lifecycle-secret-sentinel-must-not-escape" }
        }),
    )
    .await;
    assert_eq!(replay["result"]["outcome"]["status"], "replayed");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            read_frame(&mut stream)
        )
        .await
        .is_err(),
        "a replay must not publish a Target View",
    );

    let stale = request(
        &mut stream,
        "stale-delete",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000024",
            "expectedRevision": 2,
            "action": {
                "kind": "delete-provider",
                "providerId": provider["id"],
                "providerRevision": provider["providerRevision"]
            }
        }),
    )
    .await;
    assert_eq!(stale["problem"]["code"], "stale-revision");
    assert_eq!(stale["authoritativeView"]["managementRevision"], 3);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            read_frame(&mut stream)
        )
        .await
        .is_err(),
        "a failed mutation must not publish a Target View",
    );

    let deleted = request(
        &mut stream,
        "delete",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000025",
            "expectedRevision": 3,
            "action": {
                "kind": "delete-provider",
                "providerId": provider["id"],
                "providerRevision": provider["providerRevision"]
            }
        }),
    )
    .await;
    let delete_push = read_frame(&mut stream).await.unwrap();
    assert_eq!(deleted["result"]["outcome"]["status"], "applied");
    assert_eq!(delete_push["view"], deleted["result"]["outcome"]["view"]);
    assert!(
        !format!("{reordered}{replay}{stale}{deleted}{delete_push}")
            .contains("lifecycle-secret-sentinel-must-not-escape")
    );
}

#[tokio::test]
async fn duplicate_provider_is_receipt_first_and_publishes_one_secret_free_view() {
    let fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    let created = request(
        &mut stream,
        "create",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000071",
            "expectedRevision": 0,
            "action": create_action("Source", "source-secret")
        }),
    )
    .await;
    let _create_push = read_frame(&mut stream).await.unwrap();
    let source = created["result"]["outcome"]["view"]["providers"][0].clone();

    let duplicate_id = "00000000-0000-4000-8000-000000000072";
    let duplicated = request(
        &mut stream,
        "duplicate",
        json!({
            "kind": "act", "target": "codex", "actionId": duplicate_id,
            "expectedRevision": 1,
            "action": {
                "kind": "duplicate-provider",
                "sourceProviderId": source["id"],
                "sourceProviderRevision": source["providerRevision"],
                "name": "Detached Copy",
                "baseUrl": "https://copy.example/v1",
                "model": "copy-model",
                "credential": { "kind": "replace", "value": "duplicate-secret-must-not-escape" }
            }
        }),
    )
    .await;
    let duplicate_push = read_frame(&mut stream).await.unwrap();
    assert_eq!(duplicated["result"]["outcome"]["status"], "applied");
    assert_eq!(
        duplicate_push["view"],
        duplicated["result"]["outcome"]["view"]
    );
    assert_eq!(
        duplicated["result"]["outcome"]["view"]["providers"][0]["id"],
        source["id"]
    );
    assert_ne!(
        duplicated["result"]["outcome"]["view"]["providers"][1]["id"],
        source["id"]
    );
    assert!(!format!("{duplicated}{duplicate_push}").contains("duplicate-secret-must-not-escape"));

    let replay = request(
        &mut stream,
        "duplicate-replay",
        json!({
            "kind": "act", "target": "codex", "actionId": duplicate_id,
            "expectedRevision": 999,
            "action": { "malformed": "duplicate-replay-secret-must-not-escape" }
        }),
    )
    .await;
    assert_eq!(replay["result"]["outcome"]["status"], "replayed");
    assert!(
        !replay
            .to_string()
            .contains("duplicate-replay-secret-must-not-escape")
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            read_frame(&mut stream)
        )
        .await
        .is_err(),
        "a replay must not publish a Target View",
    );
}

#[tokio::test]
async fn zero_provider_revisions_are_rejected_before_mutation_but_do_not_beat_receipts() {
    let fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    let created = request(
        &mut stream,
        "create",
        json!({
            "kind": "act", "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000061",
            "expectedRevision": 0,
            "action": create_action("Provider", "zero-revision-secret")
        }),
    )
    .await;
    let _create_push = read_frame(&mut stream).await.unwrap();
    let provider = created["result"]["outcome"]["view"]["providers"][0].clone();
    let before = created["result"]["outcome"]["view"].clone();

    for (request_id, action_id, action) in [
        (
            "zero-delete",
            "00000000-0000-4000-8000-000000000062",
            json!({
                "kind": "delete-provider",
                "providerId": provider["id"],
                "providerRevision": 0,
            }),
        ),
        (
            "zero-update",
            "00000000-0000-4000-8000-000000000063",
            json!({
                "kind": "update-provider",
                "providerId": provider["id"],
                "providerRevision": 0,
                "name": "Provider",
                "baseUrl": "https://provider.example/v1",
                "model": "model-test",
                "credential": { "kind": "keep" },
            }),
        ),
    ] {
        let failure = request(
            &mut stream,
            request_id,
            json!({
                "kind": "act", "target": "codex", "actionId": action_id,
                "expectedRevision": 1, "action": action,
            }),
        )
        .await;
        assert_eq!(failure["problem"]["code"], "invalid-provider");
        assert_eq!(failure["authoritativeView"], before);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                read_frame(&mut stream)
            )
            .await
            .is_err(),
            "a rejected action must not publish a Target View",
        );
        assert!(
            fixture
                .store
                .receipt(action_id.parse().unwrap())
                .await
                .unwrap()
                .is_none(),
        );
    }

    let recorded_id = "00000000-0000-4000-8000-000000000064";
    let applied = request(
        &mut stream,
        "recorded-update",
        json!({
            "kind": "act", "target": "codex", "actionId": recorded_id,
            "expectedRevision": 1,
            "action": {
                "kind": "update-provider",
                "providerId": provider["id"],
                "providerRevision": provider["providerRevision"],
                "name": "Recorded",
                "baseUrl": "https://provider.example/v1",
                "model": "model-test",
                "credential": { "kind": "keep" },
            }
        }),
    )
    .await;
    let _update_push = read_frame(&mut stream).await.unwrap();
    assert_eq!(applied["result"]["outcome"]["status"], "applied");

    let replay = request(
        &mut stream,
        "replay-zero",
        json!({
            "kind": "act", "target": "codex", "actionId": recorded_id,
            "expectedRevision": 999,
            "action": {
                "kind": "delete-provider",
                "providerId": provider["id"],
                "providerRevision": 0,
                "sentinel": "zero-revision-must-not-escape",
            }
        }),
    )
    .await;
    assert_eq!(replay["result"]["outcome"]["status"], "replayed");
    assert!(!replay.to_string().contains("zero-revision-must-not-escape"));
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            read_frame(&mut stream)
        )
        .await
        .is_err(),
        "a replay must not publish a Target View",
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
