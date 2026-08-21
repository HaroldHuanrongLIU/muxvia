#![cfg(unix)]

use std::{
    convert::Infallible,
    fs,
    io::Write,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use axum::{Router, body::Body, response::Response, routing::post};
use fs2::FileExt;
use futures_util::{StreamExt, stream};
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
    io::AsyncReadExt,
    net::{TcpListener, UnixStream},
    process::{Child, Command},
    sync::Notify,
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

    fn user_home(&self) -> &Path {
        self.home.parent().expect("Muxvia Home has a user home")
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

struct HeldSseUpstream {
    base_url: String,
    started: Arc<Notify>,
    release: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl HeldSseUpstream {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let task = tokio::spawn({
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            async move {
                axum::serve(
                    listener,
                    Router::new().route(
                        "/v1/responses",
                        post(move || {
                            let started = Arc::clone(&started);
                            let release = Arc::clone(&release);
                            async move {
                                started.notify_one();
                                let chunks = stream::once(async {
                                    Ok::<_, Infallible>(
                                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"before\"}\n\n",
                                    )
                                })
                                .chain(stream::once(async move {
                                    release.notified().await;
                                    Ok::<_, Infallible>("data: [DONE]\n\n")
                                }));
                                Response::builder()
                                    .header("content-type", "text/event-stream")
                                    .body(Body::from_stream(chunks))
                                    .unwrap()
                            }
                        }),
                    ),
                )
                .await
                .unwrap();
            }
        });
        Self {
            base_url,
            started,
            release,
            task,
        }
    }

    async fn wait_started(&self) {
        timeout(PROCESS_TIMEOUT, self.started.notified())
            .await
            .expect("committed upstream stream did not start");
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

impl Drop for HeldSseUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn codex_routing_credential(user_home: &Path) -> String {
    let config = fs::read_to_string(user_home.join(".codex/config.toml")).unwrap();
    let marker = "\"X-Muxvia-Routing-Credential\" = \"";
    let start = config
        .find(marker)
        .map(|index| index + marker.len())
        .expect("managed configuration omitted its Routing Credential");
    config[start..]
        .split_once('"')
        .map(|(credential, _)| credential.to_owned())
        .expect("managed Routing Credential was not closed")
}

fn committed_codex_stream(
    endpoint: std::net::SocketAddr,
    routing_credential: String,
) -> tokio::task::JoinHandle<axum::body::Bytes> {
    tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://{endpoint}/v1/responses"))
            .header("X-Muxvia-Routing-Credential", routing_credential)
            .header("content-type", "application/json")
            .body(
                serde_json::to_vec(&json!({"model":"caller-model","stream":true,"input":"hello"}))
                    .unwrap(),
            )
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
    })
}

async fn assert_stream_still_committed(request: &mut tokio::task::JoinHandle<axum::body::Bytes>) {
    match timeout(Duration::from_millis(100), request).await {
        Err(_) => {}
        Ok(Ok(body)) => panic!(
            "handover completed a committed stream before its upstream released ({} bytes)",
            body.len()
        ),
        Ok(Err(_)) => panic!("handover truncated a committed stream before its upstream released"),
    }
}

async fn finish_committed_stream(request: tokio::task::JoinHandle<axum::body::Bytes>) {
    let body = timeout(PROCESS_TIMEOUT, request)
        .await
        .expect("committed stream did not finish before lifecycle transition")
        .unwrap();
    assert_eq!(
        body,
        axum::body::Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"before\"}\n\ndata: [DONE]\n\n",
        )
    );
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

fn fake_handover_candidate(root: &Path, metadata: &Value) -> PathBuf {
    let path = root.join("handover-candidate");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--lifecycle-metadata\" ]; then\n  printf '%s\\n' '{}'\n  exit 0\nfi\nexit 78\n",
            metadata
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn compatible_handover_candidate(root: &Path, release: &str) -> PathBuf {
    let path = root.join("compatible-handover-candidate");
    let binary = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/muxvia-routing")
        .canonicalize()
        .unwrap();
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--lifecycle-metadata\" ]; then\n  printf '%s\\n' '{{\"product\":\"muxvia-routing\",\"release\":\"{release}\",\"rpc\":{{\"major\":1,\"minor\":42}}}}'\n  exit 0\nfi\nexec '{}' \"$@\" --test-release '{release}'\n",
            binary.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn disappearing_handover_candidate(root: &Path, release: &str) -> PathBuf {
    let path = root.join("disappearing-handover-candidate");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--lifecycle-metadata\" ]; then\n  printf '%s\\n' '{{\"product\":\"muxvia-routing\",\"release\":\"{release}\",\"rpc\":{{\"major\":1,\"minor\":0}}}}'\n  rm -- \"$0\"\n  exit 0\nfi\nexit 78\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn hanging_handover_candidate(root: &Path) -> PathBuf {
    let path = root.join("hanging-handover-candidate");
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = \"--lifecycle-metadata\" ]; then\n  exec sleep 30\nfi\nexit 78\n",
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

async fn activate_process_takeover(
    socket: &Path,
    target: Target,
    user_home: &Path,
) -> std::net::SocketAddr {
    activate_process_takeover_for_base_url(
        socket,
        target,
        user_home,
        match target {
            Target::Codex => "https://codex-lifecycle.example/v1",
            Target::Claude => "https://claude-lifecycle.example",
        },
    )
    .await
}

async fn activate_process_takeover_for_base_url(
    socket: &Path,
    target: Target,
    user_home: &Path,
    base_url: &str,
) -> std::net::SocketAddr {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open-for-takeover",
        match target {
            Target::Codex => json!({"kind":"open-target","target":"codex"}),
            Target::Claude => json!({
                "kind":"open-target","target":"claude",
                "claudeContext":{
                    "claudeConfigDir":null,"selectorState":"unset",
                    "hostManagedState":"unmanaged","cwd":user_home
                }
            }),
        },
    )
    .await;
    let saved = request(
        &mut stream,
        "save-for-takeover",
        json!({
            "kind":"act","target":target.as_str(),"actionId":Uuid::new_v4(),
            "expectedRevision":opened["result"]["view"]["managementRevision"],
            "action":{
                "kind":"create-provider","name":format!("{} lifecycle", target.as_str()),
                "baseUrl":base_url,
                "model":"lifecycle-model",
                "credential":{"kind":"replace","value":format!("{}-lifecycle-secret", target.as_str())},
                "authentication":match target {
                    Target::Codex => "openai-bearer",
                    Target::Claude => "anthropic-api-key",
                },
                "presetKey":null
            }
        }),
    )
    .await;
    let save_push = read_frame(&mut stream).await.unwrap();
    assert!(
        save_push["type"] == "target-view",
        "save push was not delivered"
    );
    let applied = request(
        &mut stream,
        "activate-takeover",
        json!({
            "kind":"act","target":target.as_str(),"actionId":Uuid::new_v4(),
            "expectedRevision":saved["result"]["outcome"]["view"]["managementRevision"],
            "action":{
                "kind":"activate-provider",
                "providerId":saved["result"]["outcome"]["view"]["providers"][0]["id"],
                "mode":"takeover"
            }
        }),
    )
    .await;
    let activation_push = read_frame(&mut stream).await.unwrap();
    assert!(
        activation_push["type"] == "target-view",
        "activation push was not delivered"
    );
    applied["result"]["outcome"]["view"]["takeover"]["endpoint"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap()
}

async fn disable_process_takeover(socket: &Path, target: Target, user_home: &Path) -> Value {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open-for-disable",
        match target {
            Target::Codex => json!({"kind":"open-target","target":"codex"}),
            Target::Claude => json!({
                "kind":"open-target","target":"claude",
                "claudeContext":{
                    "claudeConfigDir":null,"selectorState":"unset",
                    "hostManagedState":"unmanaged","cwd":user_home
                }
            }),
        },
    )
    .await;
    let response = request(
        &mut stream,
        "disable-takeover",
        json!({
            "kind":"act","target":target.as_str(),"actionId":Uuid::new_v4(),
            "expectedRevision":opened["result"]["view"]["managementRevision"],
            "action":{"kind":"disable-takeover"}
        }),
    )
    .await;
    let push = read_frame(&mut stream).await.unwrap();
    assert!(
        response["type"] == "response",
        "disable did not return a response"
    );
    assert!(
        push["type"] == "target-view",
        "disable did not publish one Target View"
    );
    assert!(
        push["view"] == response["result"]["outcome"]["view"],
        "disable response and Target View push diverged"
    );
    response["result"]["outcome"]["view"].clone()
}

fn fingerprint(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap()
}

#[tokio::test]
async fn provider_transfer_process_tracer_proves_identity_provenance_redaction_and_atomic_rejection()
 {
    const SECRET: &str = "process-provider-import-secret-must-not-escape";
    let fixture = ProcessFixture::start().await;
    let configuration_home = fixture.user_home().join(".codex");
    fs::create_dir(&configuration_home).unwrap();
    let configuration_path = configuration_home.join("config.toml");
    let configuration = format!(
        r#"model = "gpt-process-live"
model_provider = "operator-process-live"

[model_providers.operator-process-live]
name = "Operator Process Live"
base_url = "https://process-live.example/v1/"
wire_api = "responses"
http_headers = {{ Authorization = "Bearer {SECRET}" }}
supports_websockets = false
"#
    );
    fs::write(&configuration_path, configuration.as_bytes()).unwrap();

    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let before = request(
        &mut stream,
        "provider-process-open-before",
        json!({"kind":"open-target","target":"codex"}),
    )
    .await;
    let before_view = before["result"]["view"].clone();

    let preview = request(
        &mut stream,
        "provider-process-preview-live",
        json!({
            "kind":"preview-provider-import","target":"codex",
            "source":{"kind":"live-target"}
        }),
    )
    .await;
    assert_eq!(preview["type"], "response");
    assert_eq!(
        preview["result"]["preview"]["candidates"][0]["importedCurrent"],
        true
    );
    assert!(!preview.to_string().contains(SECRET));
    let preview_token = preview["result"]["preview"]["previewToken"]
        .as_str()
        .unwrap();
    let candidate_id = preview["result"]["preview"]["candidates"][0]["candidateId"]
        .as_str()
        .unwrap();
    let confirmed = request(
        &mut stream,
        "provider-process-confirm-live",
        json!({
            "kind":"confirm-provider-import","target":"codex",
            "previewToken":preview_token,
            "choices":[{"candidateId":candidate_id,"resolution":{"kind":"create"}}]
        }),
    )
    .await;
    let provider_id = confirmed["result"]["outcome"]["records"][0]["providerId"]
        .as_str()
        .unwrap();
    assert_ne!(provider_id, candidate_id);
    let imported_push = read_frame(&mut stream).await.unwrap();
    let imported = imported_push["view"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["id"] == provider_id)
        .unwrap();
    assert_eq!(imported["completeness"], "complete");
    assert_eq!(imported["importedCurrent"], true);
    let provenance = &imported["importProvenance"];
    assert_eq!(provenance["sourceProduct"], "target-cli");
    assert_eq!(provenance["sourceTarget"], "codex");
    assert!(!provenance["sourceIdentifier"].as_str().unwrap().is_empty());
    assert_eq!(
        provenance["configurationFingerprint"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(
        imported_push["view"]["currentProviderId"],
        before_view["currentProviderId"]
    );
    assert_eq!(
        fs::read(&configuration_path).unwrap(),
        configuration.as_bytes()
    );
    assert!(!confirmed.to_string().contains(SECRET));
    assert!(!imported_push.to_string().contains(SECRET));

    let exported = request(
        &mut stream,
        "provider-process-export",
        json!({"kind":"export-provider-configuration","target":"codex"}),
    )
    .await;
    let export = exported["result"]["export"].clone();
    assert!(
        export["targetProviders"]
            .as_array()
            .unwrap()
            .iter()
            .all(|provider| provider["credential"] == "missing")
    );
    let export_text = export.to_string();
    assert!(!export_text.contains(SECRET));
    for forbidden in [
        "currentProvider",
        "servingProvider",
        "activatedSnapshot",
        "recovery",
    ] {
        assert!(!export_text.contains(forbidden));
    }

    let round_trip_preview = request(
        &mut stream,
        "provider-process-preview-export",
        json!({
            "kind":"preview-provider-import","target":"codex",
            "source":{"kind":"muxvia-export","payload":export_text}
        }),
    )
    .await;
    let round_trip_token = round_trip_preview["result"]["preview"]["previewToken"]
        .as_str()
        .unwrap();
    let round_trip_candidate =
        round_trip_preview["result"]["preview"]["candidates"][0]["candidateId"]
            .as_str()
            .unwrap();
    let round_trip = request(
        &mut stream,
        "provider-process-confirm-export",
        json!({
            "kind":"confirm-provider-import","target":"codex",
            "previewToken":round_trip_token,
            "choices":[{"candidateId":round_trip_candidate,"resolution":{"kind":"create"}}]
        }),
    )
    .await;
    let round_trip_id = round_trip["result"]["outcome"]["records"][0]["providerId"]
        .as_str()
        .unwrap();
    assert_ne!(round_trip_id, provider_id);
    assert_ne!(round_trip_id, round_trip_candidate);
    let round_trip_push = read_frame(&mut stream).await.unwrap();
    let round_trip_provider = round_trip_push["view"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["id"] == round_trip_id)
        .unwrap();
    assert_eq!(
        round_trip_provider["importProvenance"]["sourceProduct"],
        "muxvia"
    );
    assert_eq!(
        round_trip_provider["importProvenance"]["sourceTarget"],
        "universal"
    );
    assert_eq!(
        round_trip_provider["importProvenance"]["configurationFingerprint"]
            .as_str()
            .unwrap()
            .len(),
        64
    );

    let stable = request(
        &mut stream,
        "provider-process-open-stable",
        json!({"kind":"open-target","target":"codex"}),
    )
    .await;
    let rejected = request(
        &mut stream,
        "provider-process-reject-duplicate",
        json!({
            "kind":"preview-provider-import","target":"codex",
            "source":{
                "kind":"cc-switch",
                "payload":format!(
                    "ccswitch://v1/import?resource=provider&app=codex&name=Rejected&apiKey={SECRET}&apiKey=duplicate"
                )
            }
        }),
    )
    .await;
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["problem"]["code"], "duplicate-provider-import");
    assert!(!rejected.to_string().contains(SECRET));
    let after_rejection = request(
        &mut stream,
        "provider-process-open-after-rejection",
        json!({"kind":"open-target","target":"codex"}),
    )
    .await;
    assert_eq!(after_rejection["result"]["view"], stable["result"]["view"]);
    assert_eq!(
        fs::read(&configuration_path).unwrap(),
        configuration.as_bytes()
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn lifecycle_metadata_probe_is_closed_and_opens_no_product_state() {
    let root = TempDir::new().unwrap();
    let absent_home = root.path().join("must-not-exist");
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/muxvia-routing");
    let output = Command::new(binary)
        .arg("--lifecycle-metadata")
        .env_remove("HOME")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "metadata probe did not exit successfully"
    );
    assert!(
        output.stderr.is_empty(),
        "metadata probe wrote a diagnostic"
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        metadata
            == json!({
                "product":"muxvia-routing",
                "release":env!("CARGO_PKG_VERSION"),
                "rpc":{"major":1,"minor":0}
            }),
        "metadata probe returned an unexpected contract"
    );
    assert!(!absent_home.exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn invalid_handover_candidate_is_rejected_without_mutating_or_stopping_the_old_service() {
    let fixture = ProcessFixture::start().await;
    let before = fingerprint(&fixture.database());
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    for (label, metadata, expected_code) in [
        (
            "wrong-product",
            json!({
                "product":"not-muxvia","release":"next-test-release",
                "rpc":{"major":1,"minor":0}
            }),
            "invalid-handover-candidate",
        ),
        (
            "wrong-release",
            json!({
                "product":"muxvia-routing","release":"other-release",
                "rpc":{"major":1,"minor":0}
            }),
            "handover-release-mismatch",
        ),
        (
            "wrong-major",
            json!({
                "product":"muxvia-routing","release":"next-test-release",
                "rpc":{"major":2,"minor":0}
            }),
            "protocol-mismatch",
        ),
    ] {
        let candidate = fake_handover_candidate(fixture._root.path(), &metadata);
        let rejected = request(
            &mut stream,
            &format!("prepare-{label}"),
            json!({
                "kind":"prepare-handover","candidatePath":candidate,
                "expectedRelease":"next-test-release"
            }),
        )
        .await;
        assert!(
            rejected["type"] == "error" && rejected["problem"]["code"] == expected_code,
            "invalid candidate did not return its fixed handover diagnostic"
        );
        assert!(
            fingerprint(&fixture.database()) == before,
            "candidate probe mutated Routing Service state"
        );
    }
    let opened = request(
        &mut stream,
        "open-after-invalid-handover",
        json!({"kind":"open-target","target":"codex"}),
    )
    .await;
    assert!(
        opened["type"] == "response",
        "old service stopped after a rejected candidate"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn hanging_candidate_probe_is_bounded_without_mutating_or_stopping_the_old_service() {
    let fixture = ProcessFixture::start().await;
    let candidate = hanging_handover_candidate(fixture._root.path());
    let before = fingerprint(&fixture.database());
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let rejected = timeout(
        PROCESS_TIMEOUT,
        request(
            &mut stream,
            "prepare-hanging-handover",
            json!({
                "kind":"prepare-handover","candidatePath":candidate,
                "expectedRelease":"next-test-release"
            }),
        ),
    )
    .await
    .expect("candidate metadata probe exceeded the bounded lifecycle deadline");
    assert!(
        rejected["type"] == "error" && rejected["problem"]["code"] == "invalid-handover-candidate",
        "hung candidate did not return the fixed invalid candidate diagnostic"
    );
    assert!(
        fingerprint(&fixture.database()) == before,
        "hung candidate probe mutated Routing Service state"
    );
    let opened = request(
        &mut stream,
        "open-after-hanging-handover",
        json!({"kind":"open-target","target":"codex"}),
    )
    .await;
    assert_eq!(opened["type"], "response");
    fixture.shutdown().await;
}

#[tokio::test]
async fn compatible_handover_execs_in_place_and_resumes_the_same_takeover() {
    let root = TempDir::new().unwrap();
    let user_home = root.path().join("home");
    let home = user_home.join(".muxvia");
    let shutdown_file = root.path().join("shutdown");
    fs::create_dir_all(&user_home).unwrap();
    let codex = fake_cli(root.path(), "fake-codex", "codex-cli 0.106.0");
    let claude = fake_cli(root.path(), "fake-claude", "2.1.37 (Claude Code)");
    let next_release = "next-test-release";
    let candidate = compatible_handover_candidate(root.path(), next_release);
    let upstream = HeldSseUpstream::start().await;
    let mut child = command(&home, &shutdown_file)
        .arg("--test-codex-executable")
        .arg(&codex)
        .arg("--test-claude-executable")
        .arg(&claude)
        .spawn()
        .unwrap();
    let process_id = child.id().unwrap();
    let socket = home.join("run/control.sock");
    wait_for_socket(&socket).await;
    let endpoint = activate_process_takeover_for_base_url(
        &socket,
        Target::Codex,
        &user_home,
        &upstream.base_url,
    )
    .await;
    let mut committed_request =
        committed_codex_stream(endpoint, codex_routing_credential(&user_home));
    upstream.wait_started().await;

    let mut stream = UnixStream::connect(&socket).await.unwrap();
    write_frame(
        &mut stream,
        &json!({
            "type":"hello","rpc":{"major":1,"minor":0},"release":"handover-test"
        }),
    )
    .await
    .unwrap();
    let old_ack = read_frame(&mut stream).await.unwrap();
    let old_epoch = old_ack["serviceEpoch"].as_str().unwrap().to_owned();
    let prepared = request(
        &mut stream,
        "prepare-compatible-handover",
        json!({
            "kind":"prepare-handover",
            "candidatePath":candidate,
            "expectedRelease":next_release
        }),
    )
    .await;
    assert!(
        prepared["type"] == "response"
            && prepared["result"]["kind"] == "handover-prepared"
            && prepared["result"]["release"] == next_release,
        "compatible handover was not prepared"
    );
    drop(stream);
    assert_stream_still_committed(&mut committed_request).await;
    upstream.release();
    finish_committed_stream(committed_request).await;

    let (new_ack, reopened) = timeout(PROCESS_TIMEOUT, async {
        loop {
            if let Ok(mut replacement) = UnixStream::connect(&socket).await {
                let sent = write_frame(
                    &mut replacement,
                    &json!({
                        "type":"hello","rpc":{"major":1,"minor":0},
                        "release":"handover-reconnect"
                    }),
                )
                .await;
                if sent.is_ok()
                    && let Ok(ack) = read_frame(&mut replacement).await
                    && ack["release"] == next_release
                {
                    break (ack, replacement);
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("compatible replacement did not reopen the control socket");
    assert!(
        new_ack["serviceEpoch"].as_str() != Some(old_epoch.as_str()),
        "replacement did not start a new service epoch"
    );
    assert_eq!(child.id(), Some(process_id));
    let mut reopened = reopened;
    let view = request(
        &mut reopened,
        "open-after-handover",
        json!({"kind":"open-target","target":"codex"}),
    )
    .await;
    assert!(
        view["result"]["view"]["takeover"]["endpoint"]
            .as_str()
            .and_then(|value| value.trim_start_matches("http://").parse().ok())
            == Some(endpoint),
        "replacement did not resume the same Takeover endpoint"
    );
    assert!(tokio::net::TcpStream::connect(endpoint).await.is_ok());
    drop(reopened);

    fs::write(&shutdown_file, b"shutdown\n").unwrap();
    let status = timeout(PROCESS_TIMEOUT, child.wait())
        .await
        .expect("replacement did not honor the inherited shutdown path")
        .unwrap();
    assert!(status.success());
    assert!(!socket.exists());
}

#[tokio::test]
async fn failed_exec_rebinds_the_compatible_old_service_and_takeover() {
    let root = TempDir::new().unwrap();
    let user_home = root.path().join("home");
    let home = user_home.join(".muxvia");
    let shutdown_file = root.path().join("shutdown");
    fs::create_dir_all(&user_home).unwrap();
    let codex = fake_cli(root.path(), "fake-codex", "codex-cli 0.106.0");
    let claude = fake_cli(root.path(), "fake-claude", "2.1.37 (Claude Code)");
    let candidate = disappearing_handover_candidate(root.path(), "missing-next-release");
    let upstream = HeldSseUpstream::start().await;
    let mut child = command(&home, &shutdown_file)
        .arg("--test-codex-executable")
        .arg(&codex)
        .arg("--test-claude-executable")
        .arg(&claude)
        .spawn()
        .unwrap();
    let process_id = child.id().unwrap();
    let socket = home.join("run/control.sock");
    wait_for_socket(&socket).await;
    let endpoint = activate_process_takeover_for_base_url(
        &socket,
        Target::Codex,
        &user_home,
        &upstream.base_url,
    )
    .await;
    let mut committed_request =
        committed_codex_stream(endpoint, codex_routing_credential(&user_home));
    upstream.wait_started().await;

    let mut stream = UnixStream::connect(&socket).await.unwrap();
    write_frame(
        &mut stream,
        &json!({"type":"hello","rpc":{"major":1,"minor":0},"release":"failure-test"}),
    )
    .await
    .unwrap();
    let old_ack = read_frame(&mut stream).await.unwrap();
    let old_epoch = old_ack["serviceEpoch"].clone();
    let old_release = old_ack["release"].clone();
    let prepared = request(
        &mut stream,
        "prepare-disappearing-handover",
        json!({
            "kind":"prepare-handover","candidatePath":candidate,
            "expectedRelease":"missing-next-release"
        }),
    )
    .await;
    assert!(
        prepared["type"] == "response" && prepared["result"]["kind"] == "handover-prepared",
        "disappearing candidate was not prepared before the exec failure"
    );
    drop(stream);
    assert_stream_still_committed(&mut committed_request).await;
    upstream.release();
    finish_committed_stream(committed_request).await;

    let mut reopened = timeout(PROCESS_TIMEOUT, async {
        loop {
            if let Ok(mut replacement) = UnixStream::connect(&socket).await {
                let sent = write_frame(
                    &mut replacement,
                    &json!({
                        "type":"hello","rpc":{"major":1,"minor":0},
                        "release":"failure-reconnect"
                    }),
                )
                .await;
                if sent.is_ok()
                    && let Ok(ack) = read_frame(&mut replacement).await
                    && ack["release"] == old_release
                    && ack["serviceEpoch"] == old_epoch
                {
                    break replacement;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old service did not rebind after exec failure");
    assert_eq!(child.id(), Some(process_id));
    let view = request(
        &mut reopened,
        "open-after-failed-handover",
        json!({"kind":"open-target","target":"codex"}),
    )
    .await;
    assert!(
        view["result"]["view"]["takeover"]["endpoint"]
            .as_str()
            .and_then(|value| value.trim_start_matches("http://").parse().ok())
            == Some(endpoint),
        "old service did not resume the same Takeover after exec failure"
    );
    assert!(tokio::net::TcpStream::connect(endpoint).await.is_ok());
    drop(reopened);

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
async fn an_unlocked_matching_descriptor_cannot_impersonate_the_inherited_service_lock() {
    let fixture = ProcessFixture::start().await;
    let before = fingerprint(&fixture.database());
    let unlocked = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.home.join("service.lock"))
        .unwrap();
    let fd = unlocked.as_raw_fd();
    // SAFETY: fcntl operates on this live test-owned descriptor so the child can inherit it.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0);
    // SAFETY: the descriptor remains owned by `unlocked` for the entire child invocation.
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0
    );
    let impostor_shutdown = fixture._root.path().join("impostor-shutdown");
    fs::write(&impostor_shutdown, b"shutdown\n").unwrap();
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/muxvia-routing");
    let output = timeout(
        PROCESS_TIMEOUT,
        Command::new(binary)
            .arg("--home")
            .arg(&fixture.home)
            .arg("--inherited-service-lock-fd")
            .arg(fd.to_string())
            .arg("--test-shutdown-file")
            .arg(&impostor_shutdown)
            .env("MUXVIA_INTEGRATION_TEST", "1")
            .output(),
    )
    .await
    .expect("impersonating service did not terminate")
    .unwrap();

    assert_eq!(output.status.code(), Some(73));
    assert!(
        fingerprint(&fixture.database()) == before,
        "unlocked inherited descriptor reached or mutated SQLite"
    );
    let mut owner = fixture.connect().await;
    hello(&mut owner).await;
    let opened = request(
        &mut owner,
        "open-after-lock-impersonation",
        json!({"kind":"open-target","target":"codex"}),
    )
    .await;
    assert_eq!(opened["type"], "response");
    fixture.shutdown().await;
}

#[tokio::test]
async fn malformed_or_wrong_home_inherited_lock_is_rejected_before_product_state() {
    let root = TempDir::new().unwrap();
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/muxvia-routing");
    let absent_home = root.path().join("invalid-fd-home");
    fs::create_dir_all(&absent_home).unwrap();
    fs::write(absent_home.join("service.lock"), b"").unwrap();
    let malformed = Command::new(&binary)
        .arg("--home")
        .arg(&absent_home)
        .arg("--inherited-service-lock-fd")
        .arg("999999")
        .output()
        .await
        .unwrap();
    assert_eq!(malformed.status.code(), Some(73));
    assert!(!absent_home.join("state/muxvia.db").exists());
    assert!(!absent_home.join("run/control.sock").exists());

    let fixture = ProcessFixture::start().await;
    let inherited = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.home.join("service.lock"))
        .unwrap();
    let fd = inherited.as_raw_fd();
    // SAFETY: fcntl operates on this live test-owned descriptor so the child can inherit it.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0);
    // SAFETY: the descriptor remains owned by `inherited` for the child invocation.
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0
    );
    let wrong_home = root.path().join("wrong-home");
    fs::create_dir_all(&wrong_home).unwrap();
    fs::write(wrong_home.join("service.lock"), b"").unwrap();
    let lock_before = fingerprint(&wrong_home.join("service.lock"));
    let wrong = Command::new(&binary)
        .arg("--home")
        .arg(&wrong_home)
        .arg("--inherited-service-lock-fd")
        .arg(fd.to_string())
        .output()
        .await
        .unwrap();
    assert_eq!(wrong.status.code(), Some(73));
    assert_eq!(fingerprint(&wrong_home.join("service.lock")), lock_before);
    assert!(!wrong_home.join("state/muxvia.db").exists());
    assert!(!wrong_home.join("run/control.sock").exists());
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
    let root = TempDir::new().unwrap();
    let user_home = root.path().join("home");
    let home = user_home.join(".muxvia");
    let shutdown_file = root.path().join("shutdown");
    let action_started = root.path().join("action-started");
    let action_release = root.path().join("action-release");
    fs::create_dir_all(&user_home).unwrap();
    let codex = fake_cli(root.path(), "fake-codex", "codex-cli 0.106.0");
    let child = command(&home, &shutdown_file)
        .arg("--test-codex-executable")
        .arg(&codex)
        .env("MUXVIA_TEST_ACTION_STARTED_FILE", &action_started)
        .env("MUXVIA_TEST_ACTION_RELEASE_FILE", &action_release)
        .spawn()
        .unwrap();
    wait_for_socket(&home.join("run/control.sock")).await;
    let mut fixture = ProcessFixture {
        _root: root,
        home,
        shutdown_file,
        child,
    };
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
    timeout(PROCESS_TIMEOUT, async {
        while !action_started.is_file() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("service did not accept the pending action");
    drop(stream);
    assert!(
        timeout(Duration::from_millis(100), fixture.child.wait())
            .await
            .is_err(),
        "service exited before its pending action finished"
    );
    fs::write(action_release, b"release\n").unwrap();

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
async fn disabling_each_takeover_is_target_local_and_the_final_disable_exits_naturally() {
    let root = TempDir::new().unwrap();
    let user_home = root.path().join("home");
    let home = user_home.join(".muxvia");
    let shutdown_file = root.path().join("unused-shutdown");
    fs::create_dir_all(&user_home).unwrap();
    let codex_config = user_home.join(".codex/config.toml");
    let claude_settings = user_home.join(".claude/settings.json");
    fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
    fs::create_dir_all(claude_settings.parent().unwrap()).unwrap();
    let codex_before = b"# operator-owned\nunrelated = \"codex-keep\"\n".to_vec();
    let claude_before = b"{\n  \"operator\": { \"keep\": \"claude-keep\" }\n}\n".to_vec();
    fs::write(&codex_config, &codex_before).unwrap();
    fs::write(&claude_settings, &claude_before).unwrap();
    fs::set_permissions(&codex_config, fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(&claude_settings, fs::Permissions::from_mode(0o600)).unwrap();
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

    let codex_endpoint = activate_process_takeover(&socket, Target::Codex, &user_home).await;
    let claude_endpoint = activate_process_takeover(&socket, Target::Claude, &user_home).await;
    assert_ne!(codex_endpoint, claude_endpoint);

    let claude_view = disable_process_takeover(&socket, Target::Claude, &user_home).await;
    assert!(
        claude_view["mode"] == "unmanaged"
            && claude_view["takeover"]["state"] == "inactive"
            && claude_view["takeover"]["endpoint"].is_null()
            && claude_view["currentProviderId"].is_null()
            && claude_view["servingProviderId"].is_null()
            && claude_view["activatedSnapshot"].is_null()
            && claude_view["failover"]["activePlan"].is_null()
            && claude_view["failover"]["draftMembers"] == json!([]),
        "Claude disable did not clear only its Takeover state"
    );
    assert!(
        fs::read(&claude_settings).unwrap() == claude_before,
        "Claude disable did not restore exact pre-Takeover bytes"
    );
    assert_eq!(
        fs::metadata(&claude_settings).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(TcpListener::bind(claude_endpoint).await.is_ok());
    assert!(tokio::net::TcpStream::connect(codex_endpoint).await.is_ok());
    assert!(
        timeout(Duration::from_millis(200), child.wait())
            .await
            .is_err(),
        "disabling one Target stopped the peer Takeover process"
    );

    let codex_view = disable_process_takeover(&socket, Target::Codex, &user_home).await;
    assert!(
        codex_view["mode"] == "unmanaged"
            && codex_view["takeover"]["state"] == "inactive"
            && codex_view["takeover"]["endpoint"].is_null()
            && codex_view["currentProviderId"].is_null()
            && codex_view["servingProviderId"].is_null()
            && codex_view["activatedSnapshot"].is_null()
            && codex_view["failover"]["activePlan"].is_null()
            && codex_view["failover"]["draftMembers"] == json!([]),
        "final disable did not clear Codex Takeover state"
    );
    assert!(
        fs::read(&codex_config).unwrap() == codex_before,
        "Codex disable did not restore exact pre-Takeover bytes"
    );
    assert_eq!(
        fs::metadata(&codex_config).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let status = timeout(PROCESS_TIMEOUT, child.wait())
        .await
        .expect("service did not exit after its final Takeover was disabled")
        .unwrap();
    if !status.success() {
        let mut stderr = Vec::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .await
            .unwrap();
        let classification = if stderr
            .windows(b"control transport failed".len())
            .any(|window| window == b"control transport failed")
        {
            "control"
        } else if stderr
            .windows(b"state is unavailable".len())
            .any(|window| window == b"state is unavailable")
        {
            "state"
        } else {
            "unknown"
        };
        let panic_line = b"crates/routing-service/src/control/server.rs:";
        let line = stderr
            .windows(panic_line.len())
            .position(|window| window == panic_line)
            .and_then(|start| {
                let digits = &stderr[start + panic_line.len()..];
                let length = digits
                    .iter()
                    .take_while(|byte| byte.is_ascii_digit())
                    .count();
                std::str::from_utf8(&digits[..length]).ok()
            })
            .unwrap_or("none");
        panic!("service exited unsuccessfully after final disable: {classification}:{line}");
    }
    assert!(!socket.exists());
    assert!(TcpListener::bind(codex_endpoint).await.is_ok());
    assert!(!shutdown_file.exists());
}

#[tokio::test]
async fn takeover_only_native_scan_imports_without_extending_final_idle_exit() {
    let root = TempDir::new().unwrap();
    let user_home = root.path().join("home");
    let home = user_home.join(".muxvia");
    let shutdown_file = root.path().join("unused-shutdown");
    fs::create_dir_all(user_home.join(".codex")).unwrap();
    fs::write(
        user_home.join(".codex/config.toml"),
        b"# operator-owned\nunrelated = \"keep\"\n",
    )
    .unwrap();
    let codex = fake_cli(root.path(), "fake-codex", "codex-cli 0.106.0");
    let mut child = command(&home, &shutdown_file)
        .arg("--test-codex-executable")
        .arg(codex)
        .arg("--test-native-usage-scan-interval-ms")
        .arg("25")
        .spawn()
        .unwrap();
    let socket = home.join("run/control.sock");
    wait_for_socket(&socket).await;
    let _endpoint = activate_process_takeover(&socket, Target::Codex, &user_home).await;

    let sessions = user_home.join(".codex/sessions/2026/08/21");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("periodic.jsonl"),
        concat!(
            "{\"timestamp\":\"2026-08-21T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"periodic-secret-session\"}}\n",
            "{\"timestamp\":\"2026-08-21T10:00:01Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.6\"}}\n",
            "{\"timestamp\":\"2026-08-21T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":12,\"cached_input_tokens\":2,\"output_tokens\":4}}}}\n",
        ),
    )
    .unwrap();
    timeout(PROCESS_TIMEOUT, async {
        loop {
            let database =
                tokio_rusqlite::rusqlite::Connection::open(home.join("state/muxvia.db")).unwrap();
            let count = database
                .query_row("SELECT COUNT(*) FROM native_usage_records", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap();
            if count == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("periodic native usage scan did not import during Takeover");
    assert!(
        timeout(Duration::from_millis(100), child.wait())
            .await
            .is_err(),
        "an active Takeover did not keep the process alive"
    );

    disable_process_takeover(&socket, Target::Codex, &user_home).await;
    let status = timeout(PROCESS_TIMEOUT, child.wait())
        .await
        .expect("periodic scan extended final idle exit")
        .unwrap();
    assert!(status.success());
    assert!(!socket.exists());
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
            Router::new().route(
                "/v1/messages",
                post(|| async {
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"type":"message","content":[]}"#))
                        .unwrap()
                }),
            ),
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
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"type":"message","content":[]}"#
    );

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
