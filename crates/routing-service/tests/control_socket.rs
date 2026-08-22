#![cfg(unix)]

use std::{
    fs,
    future::Future,
    os::unix::fs::{FileTypeExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use muxvia_routing::{
    claude::{ClaudeCapability, ClaudeProbe, ClaudeProblem, CommandClaudeProbe},
    codex::{CodexCapability, CodexProbe, CodexProblem, CommandCodexProbe},
    control::{
        framing::{FrameError, read_frame, write_frame},
        protocol::{
            ClaudeBlockingSelector, ClaudeHostManagedState, ClaudePreflightContext,
            ClaudeSelectorState, CompatibilityClassification, ReconciliationStrategy, Target,
        },
        server::{ControlServer, ControlServerHandle, peer_uid_matches},
    },
    home::MuxviaHome,
    model::{UpstreamError, UpstreamRequest, UpstreamResponse, UpstreamTransport},
    service::activate::{ActivationHooks, ActivationPause, ActivationService},
    state::StateStore,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_rusqlite::rusqlite::params;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct ControlCodexProbe;

impl CodexProbe for ControlCodexProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        Ok(CodexCapability::Tested {
            version: "test".into(),
        })
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandCodexProbe.probe(Path::new("relative-codex")),
                result = async { Ok(CodexCapability::Tested { version: "test".into() }) } => result,
            }
        })
    }
}

struct UnknownCodexProbe(&'static str);

impl CodexProbe for UnknownCodexProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        Ok(CodexCapability::UnknownCompatible {
            version: self.0.into(),
            warning: "untested".into(),
        })
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandCodexProbe.probe(Path::new("relative-codex")),
                result = async { self.probe(Path::new("/usr/bin/codex")) } => result,
            }
        })
    }
}

struct ChangingCodexProbe {
    state: Arc<AtomicUsize>,
    calls: Option<Arc<AtomicUsize>>,
}

impl CodexProbe for ChangingCodexProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        if let Some(calls) = &self.calls {
            calls.fetch_add(1, Ordering::SeqCst);
        }
        match self.state.load(Ordering::SeqCst) {
            0 => Ok(CodexCapability::UnknownCompatible {
                version: "codex-stale-unknown".into(),
                warning: "untested".into(),
            }),
            1 => CommandCodexProbe.probe(Path::new("relative-codex")),
            _ => Ok(CodexCapability::Tested {
                version: "codex-tested-8.2".into(),
            }),
        }
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandCodexProbe.probe(Path::new("relative-codex")),
                result = async { self.probe(Path::new("/usr/bin/codex")) } => result,
            }
        })
    }
}

struct IncompatibleCodexProbe;

impl CodexProbe for IncompatibleCodexProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        CommandCodexProbe.probe(Path::new("relative-codex"))
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandCodexProbe.probe(Path::new("relative-codex")),
                result = async { self.probe(Path::new("relative-codex")) } => result,
            }
        })
    }
}

struct RuntimeSourceCodexProbe {
    expected_executable: PathBuf,
    calls: AtomicUsize,
}

struct BlockingFallbackCodexProbe {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl CodexProbe for BlockingFallbackCodexProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        let _ = self.started.send(());
        let _ = self.release.lock().unwrap().recv();
        Ok(CodexCapability::Tested {
            version: "blocking-fallback".into(),
        })
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>> {
        Box::pin(async move {
            let _ = self.started.send(());
            cancellation.cancelled().await;
            CommandCodexProbe.probe(Path::new("relative-codex"))
        })
    }
}

impl CodexProbe for RuntimeSourceCodexProbe {
    fn probe(&self, executable: &Path) -> Result<CodexCapability, CodexProblem> {
        assert_eq!(executable, self.expected_executable);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CodexCapability::Tested {
            version: "shared-runtime-codex 7.4.1".into(),
        })
    }

    fn probe_cancellable<'a>(
        &'a self,
        executable: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandCodexProbe.probe(Path::new("relative-codex")),
                result = async {
                    assert_eq!(executable, self.expected_executable);
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    Ok(CodexCapability::Tested { version: "shared-runtime-codex 7.4.1".into() })
                } => result,
            }
        })
    }
}

struct ControlClaudeProbe;

impl ClaudeProbe for ControlClaudeProbe {
    fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
        Ok(ClaudeCapability::Tested {
            version: "test".into(),
        })
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ClaudeCapability, ClaudeProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandClaudeProbe.probe(Path::new("relative-claude")),
                result = async { Ok(ClaudeCapability::Tested { version: "test".into() }) } => result,
            }
        })
    }
}

struct UnknownClaudeProbe(&'static str);

impl ClaudeProbe for UnknownClaudeProbe {
    fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
        Ok(ClaudeCapability::UnknownCompatible {
            version: self.0.into(),
            warning: "untested".into(),
        })
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ClaudeCapability, ClaudeProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandClaudeProbe.probe(Path::new("relative-claude")),
                result = async { self.probe(Path::new("/usr/bin/claude")) } => result,
            }
        })
    }
}

struct ChangingClaudeProbe {
    state: Arc<AtomicUsize>,
    calls: Option<Arc<AtomicUsize>>,
}

impl ClaudeProbe for ChangingClaudeProbe {
    fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
        if let Some(calls) = &self.calls {
            calls.fetch_add(1, Ordering::SeqCst);
        }
        match self.state.load(Ordering::SeqCst) {
            0 => Ok(ClaudeCapability::UnknownCompatible {
                version: "claude-stale-unknown".into(),
                warning: "untested".into(),
            }),
            1 => CommandClaudeProbe.probe(Path::new("relative-claude")),
            _ => Ok(ClaudeCapability::Tested {
                version: "claude-tested-8.2".into(),
            }),
        }
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ClaudeCapability, ClaudeProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandClaudeProbe.probe(Path::new("relative-claude")),
                result = async { self.probe(Path::new("/usr/bin/claude")) } => result,
            }
        })
    }
}

struct IncompatibleClaudeProbe;

impl ClaudeProbe for IncompatibleClaudeProbe {
    fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
        CommandClaudeProbe.probe(Path::new("relative-claude"))
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ClaudeCapability, ClaudeProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandClaudeProbe.probe(Path::new("relative-claude")),
                result = async { self.probe(Path::new("relative-claude")) } => result,
            }
        })
    }
}

struct CountingClaudeProbe(AtomicUsize);

impl ClaudeProbe for CountingClaudeProbe {
    fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ClaudeCapability::Tested {
            version: "test".into(),
        })
    }

    fn probe_cancellable<'a>(
        &'a self,
        _: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ClaudeCapability, ClaudeProblem>> + Send + 'a>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => CommandClaudeProbe.probe(Path::new("relative-claude")),
                result = async {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Ok(ClaudeCapability::Tested { version: "test".into() })
                } => result,
            }
        })
    }
}

struct ControlNoopUpstream;

fn assert_claude_direct_wire_is_secret_free(value: &Value) {
    let wire = value.to_string();
    for forbidden in [
        "provider-secret-must-not-escape",
        "wire-secret-must-not-escape",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
    ] {
        assert!(
            !wire.contains(forbidden),
            "Claude Direct wire surface exposed a credential or setting"
        );
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn assert_compatibility_wire_is_secret_free(value: &Value, secrets: &[&str], label: &'static str) {
    let serialized = serde_json::to_vec(value).unwrap_or_default();
    let debug = format!("{value:?}").into_bytes();
    let contaminated = secrets.iter().any(|secret| {
        contains_bytes(&serialized, secret.as_bytes()) || contains_bytes(&debug, secret.as_bytes())
    });
    assert!(!contaminated, "compatibility-wire-secret:{label}");
}

fn assert_compatibility_json_equal(
    actual: &Value,
    expected: &Value,
    secrets: &[&str],
    label: &'static str,
) {
    let contaminated = [actual, expected].into_iter().any(|value| {
        let serialized = serde_json::to_vec(value).unwrap_or_default();
        let debug = format!("{value:?}").into_bytes();
        secrets.iter().any(|secret| {
            contains_bytes(&serialized, secret.as_bytes())
                || contains_bytes(&debug, secret.as_bytes())
        })
    });
    assert!(!contaminated, "compatibility-json-secret:{label}");
    assert!(actual == expected, "compatibility-json-mismatch:{label}");
}

#[test]
fn compatibility_wire_scanner_rejects_a_controlled_mutation_with_fixed_diagnostic() {
    const SECRETS: &[&str] = &[
        "COMPATIBILITY_CREDENTIAL_SENTINEL_98001",
        "COMPATIBILITY_CONFIG_SENTINEL_98002",
        "COMPATIBILITY_BACKEND_SENTINEL_98003",
        "COMPATIBILITY_SETTINGS_SENTINEL_98004",
    ];
    let panic = std::panic::catch_unwind(|| {
        assert_compatibility_wire_is_secret_free(
            &json!({"mutated": SECRETS[2]}),
            SECRETS,
            "mutation",
        );
    })
    .unwrap_err();
    let diagnostic = panic
        .downcast_ref::<&str>()
        .map(|value| (*value).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    let fixed = diagnostic == "compatibility-wire-secret:mutation";
    let leaked = SECRETS.iter().any(|secret| diagnostic.contains(secret));
    assert!(fixed && !leaked, "compatibility-wire-scanner-diagnostic");
}

#[test]
fn target_view_json_equality_scans_same_and_opposite_operands_before_comparison() {
    const SECRET: &str = "TARGET_VIEW_JSON_EQUALITY_SECRET_98806";
    for (actual, expected) in [
        (json!({"problem": SECRET}), json!({"problem": SECRET})),
        (
            json!({"problem": SECRET}),
            json!({"problem": "fixed-safe-value"}),
        ),
    ] {
        let panic = std::panic::catch_unwind(|| {
            assert_compatibility_json_equal(&actual, &expected, &[SECRET], "mutation");
        })
        .unwrap_err();
        let diagnostic = panic
            .downcast_ref::<&str>()
            .map(|value| (*value).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let fixed = diagnostic == "compatibility-json-secret:mutation";
        let leaked = diagnostic.contains(SECRET);
        assert!(fixed && !leaked, "target-view-json-secret-diagnostic");
    }

    let mismatch = std::panic::catch_unwind(|| {
        assert_compatibility_json_equal(
            &json!({"problem": "first"}),
            &json!({"problem": "second"}),
            &[SECRET],
            "mutation",
        );
    })
    .unwrap_err();
    let mismatch_diagnostic = mismatch
        .downcast_ref::<&str>()
        .map(|value| (*value).to_owned())
        .or_else(|| mismatch.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert!(
        mismatch_diagnostic == "compatibility-json-mismatch:mutation",
        "target-view-json-mismatch-diagnostic"
    );
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

#[async_trait]
impl UpstreamTransport for ControlNoopUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        Err(UpstreamError)
    }
}

struct ControlFixture {
    root: PathBuf,
    store: Arc<StateStore>,
    handle: Option<muxvia_routing::control::server::ControlServerHandle>,
}

impl ControlFixture {
    async fn start() -> Self {
        Self::start_with_activation_hooks(ActivationHooks::default()).await
    }

    async fn start_with_activation_hooks(hooks: ActivationHooks) -> Self {
        let root = short_temp_root("mx-ctl");
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(ControlCodexProbe),
                "/usr/bin/codex".into(),
                Arc::new(ControlNoopUpstream),
            )
            .with_hooks(hooks)
            .with_claude_runtime(Arc::new(ControlClaudeProbe), "/usr/bin/claude".into()),
        );
        let handle = ControlServer::bind_with_activation(
            &home,
            Arc::clone(&store),
            "routing-test",
            activation,
        )
        .await
        .unwrap();
        Self {
            root,
            store,
            handle: Some(handle),
        }
    }

    async fn start_with_device_authority_origin(authority_origin: &str) -> Self {
        let root = short_temp_root("mx-sub");
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(ControlCodexProbe),
                "/usr/bin/codex".into(),
                Arc::new(ControlNoopUpstream),
            )
            .with_claude_runtime(Arc::new(ControlClaudeProbe), "/usr/bin/claude".into()),
        );
        let handle = ControlServer::bind_with_activation_and_device_authority_origin(
            &home,
            Arc::clone(&store),
            "routing-test",
            activation,
            authority_origin,
        )
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

struct CountingInspectionServer {
    base_url: String,
    completed: mpsc::UnboundedReceiver<()>,
    task: JoinHandle<()>,
}

impl CountingInspectionServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (completed_tx, completed) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let completed_tx = completed_tx.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let Ok(read) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                    }
                    let body = br#"{"data":[{"id":"model-complete"}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                    if stream.write_all(body).await.is_err() {
                        return;
                    }
                    let _ = completed_tx.send(());
                });
            }
        });
        Self {
            base_url,
            completed,
            task,
        }
    }

    async fn wait_completed(&mut self) {
        tokio::time::timeout(Duration::from_secs(1), self.completed.recv())
            .await
            .unwrap()
            .unwrap();
    }

    async fn assert_no_completion(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), self.completed.recv())
                .await
                .is_err(),
            "a reused request ID started a second upstream inspection"
        );
    }
}

impl Drop for CountingInspectionServer {
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

fn assert_request_history_frame_secret_free(frame: &Value, credential: &str) {
    assert!(
        !frame.to_string().contains(credential) && !format!("{frame:?}").contains(credential),
        "request-history frame exposed a designated credential"
    );
}

#[tokio::test]
async fn native_usage_lifecycle_is_target_bound_over_real_uds() {
    const SESSION_MARKER: &str = "NATIVE_USAGE_SESSION_SECRET_15001";
    let mut fixture = ControlFixture::start().await;
    let session_dir = fixture.root.join("home/.codex/sessions/2026/08/21");
    fs::create_dir_all(&session_dir).unwrap();
    let session_file = session_dir.join("source-path-secret.jsonl");
    let first = format!(
        concat!(
            "{{\"timestamp\":\"2026-08-21T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\"}}}}\n",
            "{{\"timestamp\":\"2026-08-21T10:00:01Z\",\"type\":\"turn_context\",\"payload\":{{\"model\":\"gpt-5.6\"}}}}\n",
            "{{\"timestamp\":\"2026-08-21T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"input_tokens\":12,\"cached_input_tokens\":2,\"output_tokens\":4}}}}}}}}\n",
        ),
        SESSION_MARKER
    );
    fs::write(&session_file, &first).unwrap();

    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open-native",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(opened["result"]["kind"], "target-view");
    let listed = request(
        &mut stream,
        "list-native",
        json!({ "kind": "list-usage-activity", "target": "codex", "limit": 10 }),
    )
    .await;
    assert_eq!(listed["result"]["kind"], "usage-activity-page");
    assert_eq!(
        listed["result"]["page"]["entries"][0]["kind"],
        "native-usage-record"
    );
    assert!(!listed.to_string().contains(SESSION_MARKER));
    assert!(!listed.to_string().contains("source-path-secret"));

    fs::write(
        &session_file,
        format!(
            "{first}{{\"timestamp\":\"2026-08-21T10:00:03Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"input_tokens\":6,\"cached_input_tokens\":1,\"output_tokens\":2}}}}}}}}\n"
        ),
    )
    .unwrap();
    let refreshed = request(
        &mut stream,
        "refresh-native",
        json!({ "kind": "refresh-native-usage", "target": "codex" }),
    )
    .await;
    assert_eq!(refreshed["result"]["refresh"]["importedRecords"], 1);
    let retained = request(
        &mut stream,
        "retain-native",
        json!({ "kind": "set-usage-retention", "target": "codex", "detailedRetentionDays": 14 }),
    )
    .await;
    assert_eq!(retained["result"]["outcome"]["detailedRetentionDays"], 14);
    let cleared = request(
        &mut stream,
        "clear-native",
        json!({ "kind": "clear-usage", "target": "codex" }),
    )
    .await;
    assert_eq!(cleared["result"]["outcome"]["clearedNativeUsageRecords"], 2);

    fixture.shutdown().await;
}

#[tokio::test]
async fn request_history_pages_and_details_are_target_bound_over_real_uds() {
    const HISTORY_CREDENTIAL: &str = "REQUEST_HISTORY_CREDENTIAL_SECRET_14001";
    let mut fixture = ControlFixture::start().await;
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    let database = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap();
    let recent_base = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
        - 1_000;
    database
        .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
            connection.execute(
                "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, 'codex', ?2)",
                params!["00000000-0000-4000-8000-000000001400", HISTORY_CREDENTIAL],
            )?;
            for (id, target, finished, outcome, status, payload) in [
                (
                    "00000000-0000-4000-8000-000000001400",
                    "codex",
                    recent_base + 100,
                    "route-unavailable",
                    None,
                    None,
                ),
                (
                    "00000000-0000-4000-8000-000000001401",
                    "codex",
                    recent_base + 110,
                    "success",
                    Some(200_i64),
                    None,
                ),
                (
                    "00000000-0000-4000-8000-000000001402",
                    "claude",
                    recent_base + 120,
                    "upstream-error",
                    Some(429),
                    Some(b"claude sanitized failure".as_slice()),
                ),
                (
                    "00000000-0000-4000-8000-000000001403",
                    "codex",
                    recent_base + 130,
                    "upstream-error",
                    Some(429),
                    Some(b"codex sanitized failure".as_slice()),
                ),
                (
                    "00000000-0000-4000-8000-000000001404",
                    "codex",
                    recent_base + 140,
                    "success",
                    Some(200),
                    None,
                ),
            ] {
                connection.execute(
                    "INSERT INTO request_records
                       (id, target, plan_id, plan_epoch, provider_id, provider_name, model,
                        protocol, started_at_unix_ms, finished_at_unix_ms, latency_ms,
                        outcome, http_status, usage_observed, input_tokens,
                        cached_input_tokens, cache_creation_input_tokens, output_tokens,
                        error_payload, error_payload_truncated)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'History Provider', 'history-model',
                             ?6, ?7, ?8, 10, ?9, ?10, 0, 0, 0, 0, 0, ?11, ?12)",
                    params![
                        id,
                        target,
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        if target == "codex" {
                            "openai-responses"
                        } else {
                            "anthropic-messages"
                        },
                        finished - 10,
                        finished,
                        outcome,
                        status,
                        payload,
                        i64::from(id.ends_with("1403")),
                    ],
                )?;
            }
            connection.execute(
                "INSERT INTO pricing_snapshots
                   (request_record_id, catalog_version, source, source_model,
                    input_nano_usd_per_million, output_nano_usd_per_million,
                    cache_read_multiplier_ppm, cache_creation_multiplier_ppm,
                    priced_at_unix_ms, estimated_cost_nano_usd)
                 VALUES (?1, 'history-catalog-v1', 'history-test', 'history-model',
                         1, 2, 3, 4, ?2, 5)",
                params!["00000000-0000-4000-8000-000000001403", recent_base + 130,],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let mut session = fixture.connect().await;
    hello(&mut session).await;
    let opened = request(
        &mut session,
        "history-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_request_history_frame_secret_free(&opened, HISTORY_CREDENTIAL);
    assert!(
        opened["type"] == "response",
        "request history owning Target did not open"
    );
    let first = request(
        &mut session,
        "history-first",
        json!({
            "kind": "list-request-records", "target": "codex",
            "beforeCursor": null, "limit": 2
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&first, HISTORY_CREDENTIAL);
    assert!(
        !first.to_string().contains("codex sanitized failure"),
        "request history page exposed retained failed payload bytes"
    );
    assert!(
        first["type"] == "response"
            && first["result"]["kind"] == "request-record-page"
            && first["result"]["page"]["target"] == "codex"
            && first["result"]["page"]["records"]
                .as_array()
                .is_some_and(|records| {
                    records.len() == 2
                        && records[0]["id"] == "00000000-0000-4000-8000-000000001404"
                        && records[1]["id"] == "00000000-0000-4000-8000-000000001403"
                })
            && first["result"]["page"]["nextCursor"].is_string(),
        "request history did not return a newest-first bounded Target page"
    );
    let cursor = first["result"]["page"]["nextCursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = request(
        &mut session,
        "history-second",
        json!({
            "kind": "list-request-records", "target": "codex",
            "beforeCursor": cursor, "limit": 2
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&second, HISTORY_CREDENTIAL);
    assert!(
        second["result"]["page"]["records"]
            .as_array()
            .is_some_and(|records| {
                records.len() == 2
                    && records[0]["id"] == "00000000-0000-4000-8000-000000001401"
                    && records[1]["id"] == "00000000-0000-4000-8000-000000001400"
            })
            && second["result"]["page"]["nextCursor"].is_null(),
        "request history cursor skipped or repeated a Target record"
    );
    let detail = request(
        &mut session,
        "history-detail",
        json!({
            "kind": "inspect-request-record", "target": "codex",
            "recordId": "00000000-0000-4000-8000-000000001403"
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&detail, HISTORY_CREDENTIAL);
    assert!(
        detail["type"] == "response"
            && detail["result"]["kind"] == "request-record-detail"
            && detail["result"]["detail"]["target"] == "codex"
            && detail["result"]["detail"]["record"]["id"] == "00000000-0000-4000-8000-000000001403"
            && detail["result"]["detail"]["errorPayload"] == "codex sanitized failure"
            && detail["result"]["detail"]["errorPayloadSensitive"] == true
            && detail["result"]["detail"]["record"]["errorPayloadTruncated"] == true
            && detail["result"]["detail"]["pricingSnapshot"]["catalogVersion"]
                == "history-catalog-v1"
            && detail["result"]["detail"]["pricingSnapshot"]["estimatedCostNanoUsd"] == 5,
        "request history detail did not return the exact Target-bound failed record"
    );
    let payload_free_failure = request(
        &mut session,
        "history-payload-free-failure",
        json!({
            "kind": "inspect-request-record", "target": "codex",
            "recordId": "00000000-0000-4000-8000-000000001400"
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&payload_free_failure, HISTORY_CREDENTIAL);
    assert!(
        payload_free_failure["type"] == "response"
            && payload_free_failure["result"]["kind"] == "request-record-detail"
            && payload_free_failure["result"]["detail"]["record"]["outcome"] == "route-unavailable"
            && payload_free_failure["result"]["detail"]["errorPayload"].is_null()
            && payload_free_failure["result"]["detail"]["errorPayloadSensitive"] == false,
        "payload-free failed Request Record was not inspectable"
    );
    let successful_detail = request(
        &mut session,
        "history-success-detail",
        json!({
            "kind": "inspect-request-record", "target": "codex",
            "recordId": "00000000-0000-4000-8000-000000001404"
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&successful_detail, HISTORY_CREDENTIAL);
    assert!(
        successful_detail["problem"]["code"] == "request-record-not-found",
        "successful Request Record exposed an inspection result"
    );
    let missing = request(
        &mut session,
        "history-missing",
        json!({
            "kind": "inspect-request-record", "target": "codex",
            "recordId": "00000000-0000-4000-8000-000000001499"
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&missing, HISTORY_CREDENTIAL);
    assert!(
        missing["problem"]["code"] == "request-record-not-found",
        "missing request history detail returned an unstable problem"
    );
    let malformed_cursor = request(
        &mut session,
        "history-malformed-cursor",
        json!({
            "kind": "list-request-records", "target": "codex",
            "beforeCursor": "not-a-request-history-cursor", "limit": 2
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&malformed_cursor, HISTORY_CREDENTIAL);
    assert!(
        malformed_cursor["problem"]["code"] == "invalid-request-history-cursor",
        "malformed request history cursor returned an unstable problem"
    );
    drop(session);
    let mut claude = fixture.connect().await;
    hello(&mut claude).await;
    let claude_open = request(
        &mut claude,
        "history-open-claude",
        json!({
            "kind": "open-target", "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "unset",
                "hostManagedState": "unmanaged",
                "cwd": fixture.root.join("home")
            }
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&claude_open, HISTORY_CREDENTIAL);
    assert!(
        claude_open["type"] == "response",
        "request history peer Target did not reopen"
    );
    let wrong_target = request(
        &mut claude,
        "history-wrong-target",
        json!({
            "kind": "inspect-request-record", "target": "claude",
            "recordId": "00000000-0000-4000-8000-000000001403"
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&wrong_target, HISTORY_CREDENTIAL);
    assert!(
        wrong_target["problem"]["code"] == "request-record-not-found",
        "request history detail crossed its Target boundary"
    );
    let cross_target_cursor = request(
        &mut claude,
        "history-cross-target-cursor",
        json!({
            "kind": "list-request-records", "target": "claude",
            "beforeCursor": first["result"]["page"]["nextCursor"], "limit": 2
        }),
    )
    .await;
    assert_request_history_frame_secret_free(&cross_target_cursor, HISTORY_CREDENTIAL);
    assert!(
        cross_target_cursor["problem"]["code"] == "invalid-request-history-cursor",
        "request history cursor crossed its Target boundary"
    );

    drop(claude);
    fixture.shutdown().await;
}

#[tokio::test]
async fn subscription_device_authorization_survives_disconnect_and_responds_before_one_push() {
    let authority = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", authority.local_addr().unwrap());
    let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured_requests = Arc::clone(&captured);
    let authority_task = tokio::spawn(async move {
        for (status, body) in [
            (
                "200 OK",
                r#"{"device_auth_id":"REMOTE_DEVICE_UDS_SECRET_11801","user_code":"WXYZ-1234","interval":5,"expires_in":900}"#,
            ),
            ("403 Forbidden", ""),
            (
                "200 OK",
                r#"{"authorization_code":"AUTHORIZATION_UDS_SECRET_11802","code_verifier":"SERVER_VERIFIER_UDS_SECRET_11803"}"#,
            ),
            (
                "200 OK",
                concat!(
                    "{\"access_token\":\"ACCESS_TOKEN_UDS_SECRET_11804\",",
                    "\"refresh_token\":\"REFRESH_TOKEN_UDS_SECRET_11805\",",
                    "\"id_token\":\"e30.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NvdW50LXVkcyIsImVtYWlsIjoib3BlcmF0b3JAZXhhbXBsZS50ZXN0In0.signature\",",
                    "\"expires_in\":3600}"
                ),
            ),
        ] {
            let (mut socket, _) = authority.accept().await.unwrap();
            let request = read_subscription_http_request(&mut socket).await;
            captured_requests.lock().unwrap().push(request);
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let mut fixture = ControlFixture::start_with_device_authority_origin(&origin).await;
    let mut initiator = fixture.connect().await;
    let mut subscriber = fixture.connect().await;
    hello(&mut initiator).await;
    hello(&mut subscriber).await;
    for (stream, label) in [
        (&mut initiator, "initiator"),
        (&mut subscriber, "subscriber"),
    ] {
        let opened = request(
            stream,
            &format!("open-{label}"),
            json!({"kind": "open-subscription-accounts"}),
        )
        .await;
        assert!(
            opened["result"]["kind"] == "subscription-account-catalog",
            "subscription account session did not open"
        );
    }
    let started = request(
        &mut initiator,
        "start-device",
        json!({
            "kind": "start-device-authorization",
            "reauthorizeAccountId": null
        }),
    )
    .await;
    let started_text = serde_json::to_string(&started).unwrap();
    for secret in [
        "REMOTE_DEVICE_UDS_SECRET_11801",
        "SERVER_VERIFIER_UDS_SECRET_11803",
        "ACCESS_TOKEN_UDS_SECRET_11804",
        "REFRESH_TOKEN_UDS_SECRET_11805",
    ] {
        assert!(
            !started_text.contains(secret),
            "device authorization start frame exposed a private value"
        );
    }
    let flow_id = started["result"]["challenge"]["flowId"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(initiator);

    let mut poller = fixture.connect().await;
    hello(&mut poller).await;
    request(
        &mut poller,
        "open-poller",
        json!({"kind": "open-subscription-accounts"}),
    )
    .await;
    let pending = request(
        &mut poller,
        "poll-pending",
        json!({"kind": "poll-device-authorization", "flowId": flow_id}),
    )
    .await;
    assert!(
        pending["result"]["poll"]["status"] == "pending",
        "pending device authorization changed state"
    );
    let authorized = request(
        &mut poller,
        "poll-authorized",
        json!({"kind": "poll-device-authorization", "flowId": flow_id}),
    )
    .await;
    assert!(
        authorized["result"]["poll"]["status"] == "authorized",
        "authorized device poll returned the wrong result"
    );
    let poller_push = read_frame(&mut poller).await.unwrap();
    let subscriber_push = read_frame(&mut subscriber).await.unwrap();
    assert!(
        poller_push["type"] == "subscription-account-view"
            && subscriber_push == poller_push
            && poller_push["view"]["revision"] == 1
            && poller_push["view"]["accounts"][0]["accountId"] == "account-uds",
        "authorized poll did not publish exactly the committed account catalog"
    );
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    let private_file = fs::read_to_string(home.subscription_accounts_path()).unwrap();
    assert!(
        private_file.contains("REFRESH_TOKEN_UDS_SECRET_11805")
            && !private_file.contains("ACCESS_TOKEN_UDS_SECRET_11804")
            && !private_file.contains("SERVER_VERIFIER_UDS_SECRET_11803"),
        "private account file stored the wrong token material"
    );
    authority_task.await.unwrap();
    let pinned_contract_observed = {
        let requests = captured.lock().unwrap();
        requests.len() == 4
            && requests[3].contains("code_verifier=SERVER_VERIFIER_UDS_SECRET_11803")
    };
    assert!(
        pinned_contract_observed,
        "real UDS flow departed from the pinned remote contract"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn cancelled_subscription_poll_cannot_exchange_or_persist_late_authorization() {
    let authority = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", authority.local_addr().unwrap());
    let (poll_seen_tx, poll_seen_rx) = oneshot::channel();
    let (release_poll_tx, release_poll_rx) = oneshot::channel();
    let exchange_count = Arc::new(AtomicUsize::new(0));
    let authority_exchange_count = Arc::clone(&exchange_count);
    let authority_task = tokio::spawn(async move {
        let (mut start, _) = authority.accept().await.unwrap();
        let _ = read_subscription_http_request(&mut start).await;
        let start_body = r#"{"device_auth_id":"CANCELLED_REMOTE_DEVICE_SECRET_11831","user_code":"CANCEL-1234","interval":5,"expires_in":900}"#;
        start
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{start_body}",
                    start_body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let (mut poll, _) = authority.accept().await.unwrap();
        let _ = read_subscription_http_request(&mut poll).await;
        let _ = poll_seen_tx.send(());
        let _ = release_poll_rx.await;
        let poll_body = r#"{"authorization_code":"CANCELLED_AUTHORIZATION_SECRET_11832","code_verifier":"CANCELLED_VERIFIER_SECRET_11833"}"#;
        let _ = poll
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{poll_body}",
                    poll_body.len()
                )
                .as_bytes(),
            )
            .await;

        if let Ok(Ok((mut exchange, _))) =
            tokio::time::timeout(Duration::from_millis(250), authority.accept()).await
        {
            authority_exchange_count.fetch_add(1, Ordering::SeqCst);
            let _ = read_subscription_http_request(&mut exchange).await;
            let token_body = concat!(
                "{\"access_token\":\"CANCELLED_ACCESS_SECRET_11834\",",
                "\"refresh_token\":\"CANCELLED_REFRESH_SECRET_11835\",",
                "\"id_token\":\"e30.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NvdW50LWNhbmNlbGxlZCJ9.signature\",",
                "\"expires_in\":3600}"
            );
            let _ = exchange
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{token_body}",
                        token_body.len()
                    )
                    .as_bytes(),
                )
                .await;
        }
    });

    let mut fixture = ControlFixture::start_with_device_authority_origin(&origin).await;
    let mut poller = fixture.connect().await;
    hello(&mut poller).await;
    request(
        &mut poller,
        "open-cancelled-poll",
        json!({"kind": "open-subscription-accounts"}),
    )
    .await;
    let started = request(
        &mut poller,
        "start-cancelled-poll",
        json!({
            "kind": "start-device-authorization",
            "reauthorizeAccountId": null
        }),
    )
    .await;
    let flow_id = started["result"]["challenge"]["flowId"]
        .as_str()
        .unwrap()
        .to_owned();
    write_frame(
        &mut poller,
        &json!({
            "type": "request",
            "requestId": "cancelled-poll",
            "operation": {"kind": "poll-device-authorization", "flowId": flow_id}
        }),
    )
    .await
    .unwrap();
    poll_seen_rx.await.unwrap();
    write_frame(
        &mut poller,
        &json!({"type": "cancel", "requestId": "cancelled-poll"}),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if fixture.handle.as_ref().unwrap().tracked_inspections() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled account poll did not leave inspection tracking");
    let _ = release_poll_tx.send(());
    authority_task.await.unwrap();

    let mut observer = fixture.connect().await;
    hello(&mut observer).await;
    let catalog = request(
        &mut observer,
        "open-after-cancelled-poll",
        json!({"kind": "open-subscription-accounts"}),
    )
    .await;
    assert!(
        exchange_count.load(Ordering::SeqCst) == 0
            && catalog["result"]["view"]["revision"] == 0
            && catalog["result"]["view"]["accounts"] == json!([]),
        "cancelled device poll exchanged or persisted a late authorization"
    );
    fixture.shutdown().await;
}

async fn read_subscription_http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read != 0, "subscription authority request ended early");
        bytes.extend_from_slice(&chunk[..read]);
        let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + content_length {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

#[tokio::test]
async fn subscription_bindings_default_preview_delete_and_replay_are_authoritative() {
    let mut fixture = ControlFixture::start().await;
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    fs::create_dir_all(home.subscription_accounts_path().parent().unwrap()).unwrap();
    fs::write(
        home.subscription_accounts_path(),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "accounts": {
                "account-primary": {
                    "account_id": "account-primary",
                    "email": "primary@example.test",
                    "refresh_token": "ACCOUNT_PRIMARY_SECRET_11811",
                    "authenticated_at": 1,
                    "state": "authorized"
                },
                "account-secondary": {
                    "account_id": "account-secondary",
                    "email": "secondary@example.test",
                    "refresh_token": "ACCOUNT_SECONDARY_SECRET_11812",
                    "authenticated_at": 2,
                    "state": "authorized"
                }
            },
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
    let provider = fixture
        .store
        .apply_provider_action_for(
            Target::Codex,
            Uuid::new_v4(),
            0,
            json!({
                "kind": "create-provider",
                "name": "Subscription metadata",
                "baseUrl": "https://example.test/v1",
                "model": "subscription-model",
                "credential": {"kind": "replace", "value": "PROVIDER_SECRET_11813"},
                "authentication": "openai-bearer",
                "presetKey": null
            }),
        )
        .await
        .unwrap()
        .view
        .providers
        .remove(0);
    let mut initiator = fixture.connect().await;
    let mut subscriber = fixture.connect().await;
    hello(&mut initiator).await;
    hello(&mut subscriber).await;
    for (stream, label) in [
        (&mut initiator, "initiator"),
        (&mut subscriber, "subscriber"),
    ] {
        request(
            stream,
            &format!("open-binding-{label}"),
            json!({"kind": "open-subscription-accounts"}),
        )
        .await;
    }

    let fixed_action_id = Uuid::new_v4();
    let fixed_action = json!({
        "kind": "bind-provider-fixed",
        "target": "codex",
        "providerId": provider.id,
        "providerRevision": provider.provider_revision,
        "accountId": "account-primary"
    });
    let fixed = request(
        &mut initiator,
        "bind-fixed",
        json!({
            "kind": "subscription-account-act",
            "actionId": fixed_action_id,
            "expectedRevision": 0,
            "action": fixed_action
        }),
    )
    .await;
    let fixed_push = read_frame(&mut initiator).await.unwrap();
    let fixed_subscriber = read_frame(&mut subscriber).await.unwrap();
    assert!(
        fixed["result"]["outcome"]["view"]["revision"] == 1
            && fixed_push["view"] == fixed["result"]["outcome"]["view"]
            && fixed_subscriber == fixed_push,
        "fixed binding response/push ordering changed"
    );

    let deleted = request(
        &mut initiator,
        "delete-account",
        json!({
            "kind": "subscription-account-act",
            "actionId": Uuid::new_v4(),
            "expectedRevision": 1,
            "action": {"kind": "delete-account", "accountId": "account-primary"}
        }),
    )
    .await;
    let delete_push = read_frame(&mut initiator).await.unwrap();
    let delete_subscriber = read_frame(&mut subscriber).await.unwrap();
    assert!(
        deleted["result"]["outcome"]["view"]["bindings"][0]["binding"]["accountId"]
            == "account-primary"
            && deleted["result"]["outcome"]["view"]["bindings"][0]["resolution"]["state"]
                == "missing"
            && deleted["result"]["outcome"]["view"]["defaultAccountId"] == "account-secondary"
            && delete_push["view"] == deleted["result"]["outcome"]["view"]
            && delete_subscriber == delete_push,
        "deleting a fixed account substituted or removed its binding"
    );

    let followed = request(
        &mut initiator,
        "bind-follow",
        json!({
            "kind": "subscription-account-act",
            "actionId": Uuid::new_v4(),
            "expectedRevision": 2,
            "action": {
                "kind": "bind-provider-follow-default",
                "target": "codex",
                "providerId": provider.id,
                "providerRevision": provider.provider_revision
            }
        }),
    )
    .await;
    let _follow_push = read_frame(&mut initiator).await.unwrap();
    let _follow_subscriber = read_frame(&mut subscriber).await.unwrap();
    assert!(
        followed["result"]["outcome"]["view"]["bindings"][0]["resolution"]["state"] == "available",
        "follow-default binding did not resolve the deterministic fallback default"
    );
    let preview = request(
        &mut initiator,
        "preview-default",
        json!({
            "kind": "preview-default-subscription-account",
            "accountId": "account-secondary"
        }),
    )
    .await;
    assert!(
        preview["result"]["preview"]["effects"][0]["currentAccountId"] == "account-secondary"
            && preview["result"]["preview"]["effects"][0]["nextAccountId"] == "account-secondary"
            && preview["result"]["preview"]["effects"][0]["nextResolution"] == "available",
        "default preview omitted the old and new resolved account identities"
    );
    let default_action_id = Uuid::new_v4();
    let default_action = json!({
        "kind": "set-default-account",
        "accountId": "account-secondary",
        "previewToken": preview["result"]["preview"]["previewToken"]
    });
    let applied = request(
        &mut initiator,
        "set-default",
        json!({
            "kind": "subscription-account-act",
            "actionId": default_action_id,
            "expectedRevision": 3,
            "action": default_action
        }),
    )
    .await;
    let _default_push = read_frame(&mut initiator).await.unwrap();
    let _default_subscriber = read_frame(&mut subscriber).await.unwrap();
    assert!(
        applied["result"]["outcome"]["view"]["defaultAccountId"] == "account-secondary"
            && applied["result"]["outcome"]["view"]["bindings"][0]["resolution"]["state"]
                == "available",
        "default confirmation did not apply the previewed resolution"
    );
    let replay = request(
        &mut initiator,
        "set-default-replay",
        json!({
            "kind": "subscription-account-act",
            "actionId": default_action_id,
            "expectedRevision": 999,
            "action": default_action
        }),
    )
    .await;
    assert!(
        replay["result"]["outcome"]["status"] == "replayed"
            && replay["result"]["outcome"]["view"] == applied["result"]["outcome"]["view"],
        "default action replay was not receipt-first"
    );
    let malformed_replay = request(
        &mut initiator,
        "set-default-malformed-replay",
        json!({
            "kind": "subscription-account-act",
            "actionId": default_action_id,
            "expectedRevision": 1000,
            "action": {"kind": "malformed-replay"}
        }),
    )
    .await;
    assert!(
        malformed_replay["result"]["outcome"]["status"] == "replayed"
            && malformed_replay["result"]["outcome"]["view"]
                == applied["result"]["outcome"]["view"],
        "malformed Subscription Account replay was parsed before its durable receipt"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut subscriber))
            .await
            .is_err(),
        "receipt replay published a duplicate catalog view"
    );
    let stale = request(
        &mut initiator,
        "stale-account-delete",
        json!({
            "kind": "subscription-account-act",
            "actionId": Uuid::new_v4(),
            "expectedRevision": 3,
            "action": {"kind": "delete-account", "accountId": "account-secondary"}
        }),
    )
    .await;
    assert!(
        stale["problem"]["code"] == "stale-subscription-catalog-revision"
            && stale["authoritativeSubscriptionAccountView"]["revision"] == 4
            && stale["authoritativeSubscriptionAccountView"]["accounts"]
                .as_array()
                .is_some_and(|accounts| accounts.len() == 1),
        "stale Subscription Account action did not return the authoritative catalog"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut subscriber))
            .await
            .is_err(),
        "stale Subscription Account action published a catalog view"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn subscription_account_writer_failure_suppresses_publication_but_reopen_reads_durable_state()
{
    let mut fixture = ControlFixture::start().await;
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    let accounts = (0..1_024)
        .map(|index| {
            let account_id = format!("account-{index:04}");
            (
                account_id.clone(),
                json!({
                    "account_id": account_id,
                    "email": format!("operator-{index:04}@example.test"),
                    "refresh_token": format!("PRIVATE_ACCOUNT_WRITER_SECRET_{index:04}"),
                    "authenticated_at": index + 1,
                    "state": "authorized"
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    fs::write(
        home.subscription_accounts_path(),
        serde_json::to_vec(&json!({
            "version": 1,
            "accounts": accounts,
            "default_account_id": "account-0000"
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(
        home.subscription_accounts_path(),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();

    let mut initiator = fixture.connect().await;
    let mut subscriber = fixture.connect().await;
    hello(&mut initiator).await;
    hello(&mut subscriber).await;
    for (stream, request_id) in [
        (&mut initiator, "account-writer-initiator"),
        (&mut subscriber, "account-writer-subscriber"),
    ] {
        request(
            stream,
            request_id,
            json!({"kind": "open-subscription-accounts"}),
        )
        .await;
    }

    for index in 0..8 {
        write_frame(
            &mut initiator,
            &json!({
                "type": "request",
                "requestId": format!("account-writer-fill-{index}"),
                "operation": {"kind": "open-subscription-accounts"}
            }),
        )
        .await
        .unwrap();
    }
    let action_id = Uuid::new_v4();
    write_frame(
        &mut initiator,
        &json!({
            "type": "request",
            "requestId": "account-writer-delete",
            "operation": {
                "kind": "subscription-account-act",
                "actionId": action_id,
                "expectedRevision": 0,
                "action": {"kind": "delete-account", "accountId": "account-0000"}
            }
        }),
    )
    .await
    .unwrap();

    let database = tokio_rusqlite::rusqlite::Connection::open(home.database_path()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let committed: bool = database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM subscription_account_action_receipts WHERE action_id = ?1)",
                    [action_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            if committed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Subscription Account action did not commit behind the blocked writer");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(40), read_frame(&mut subscriber))
            .await
            .is_err(),
        "failed Subscription Account response writer emitted a misleading catalog push"
    );

    let mut reopened = fixture.connect().await;
    hello(&mut reopened).await;
    let visible = request(
        &mut reopened,
        "account-open-after-writer-failure",
        json!({"kind": "open-subscription-accounts"}),
    )
    .await;
    assert!(
        visible["result"]["view"]["revision"] == 1
            && visible["result"]["view"]["defaultAccountId"] == "account-1023"
            && visible["result"]["view"]["accounts"]
                .as_array()
                .is_some_and(|values| values.len() == 1_023),
        "reopen did not read the durable Subscription Account state after writer failure"
    );
    drop(initiator);
    fixture.shutdown().await;
}

#[tokio::test]
async fn universal_provider_catalog_sessions_respond_before_one_push_and_replay_without_push() {
    let fixture = ControlFixture::start().await;
    let mut initiator = fixture.connect().await;
    let mut subscriber = fixture.connect().await;
    hello(&mut initiator).await;
    hello(&mut subscriber).await;

    for (stream, request_id) in [
        (&mut initiator, "open-universal-initiator"),
        (&mut subscriber, "open-universal-subscriber"),
    ] {
        let opened = request(
            stream,
            request_id,
            json!({ "kind": "open-universal-providers" }),
        )
        .await;
        assert_eq!(
            opened["result"]["kind"].as_str(),
            Some("universal-provider-catalog"),
            "opening a Universal Provider catalog session returned the wrong result",
        );
        assert_eq!(
            opened["result"]["view"]["revision"].as_u64(),
            Some(0),
            "a fresh Universal Provider catalog did not start at revision zero",
        );
    }

    let action_id = "00000000-0000-4000-8000-000000000901";
    write_frame(
        &mut initiator,
        &json!({
            "type": "request",
            "requestId": "create-universal",
            "operation": {
                "kind": "universal-provider-act",
                "actionId": action_id,
                "expectedRevision": 0,
                "action": {
                    "kind": "create-universal-provider",
                    "name": "Shared Gateway",
                    "baseUrl": "https://universal-session.example/v1",
                    "credential": {
                        "kind": "replace",
                        "value": "UNIVERSAL_SESSION_CREDENTIAL_901"
                    },
                    "presetKey": null,
                    "targets": [
                        {
                            "target": "codex",
                            "enabled": true,
                            "model": "shared-model",
                            "authentication": "openai-bearer",
                            "routingRequirement": "direct-compatible"
                        },
                        {
                            "target": "claude",
                            "enabled": true,
                            "model": "shared-model",
                            "authentication": "anthropic-bearer",
                            "routingRequirement": "takeover-required"
                        }
                    ]
                }
            }
        }),
    )
    .await
    .unwrap();

    let response = read_frame(&mut initiator).await.unwrap();
    assert!(
        !response
            .to_string()
            .contains("UNIVERSAL_SESSION_CREDENTIAL_901"),
        "Universal Provider action response exposed a credential",
    );
    assert_eq!(
        response["result"]["kind"].as_str(),
        Some("universal-provider-outcome"),
        "Universal Provider action did not return an outcome",
    );
    assert_eq!(
        response["result"]["outcome"]["status"].as_str(),
        Some("applied"),
        "Universal Provider action was not applied",
    );
    let initiating_push = read_frame(&mut initiator).await.unwrap();
    let subscriber_push = read_frame(&mut subscriber).await.unwrap();
    for push in [&initiating_push, &subscriber_push] {
        assert!(
            !push
                .to_string()
                .contains("UNIVERSAL_SESSION_CREDENTIAL_901"),
            "Universal Provider catalog push exposed a credential",
        );
        assert_eq!(
            push["type"].as_str(),
            Some("universal-provider-view"),
            "Universal Provider catalog subscriber received the wrong push",
        );
        assert_eq!(
            push["view"]["viewSequence"].as_u64(),
            Some(1),
            "Universal Provider catalog push carried the wrong sequence",
        );
    }

    let replay = request(
        &mut initiator,
        "replay-universal",
        json!({
            "kind": "universal-provider-act",
            "actionId": action_id,
            "expectedRevision": 999,
            "action": { "malformed": "UNIVERSAL_REPLAY_SECRET_902" }
        }),
    )
    .await;
    assert!(
        !replay.to_string().contains("UNIVERSAL_REPLAY_SECRET_902"),
        "Universal Provider receipt replay exposed malformed action input",
    );
    assert_eq!(
        replay["result"]["outcome"]["status"].as_str(),
        Some("replayed"),
        "Universal Provider receipt did not replay before action parsing",
    );
    for stream in [&mut initiator, &mut subscriber] {
        assert!(
            tokio::time::timeout(Duration::from_millis(80), read_frame(stream))
                .await
                .is_err(),
            "Universal Provider receipt replay published a duplicate catalog view",
        );
    }

    let provider_id = response["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .expect("created Universal Provider did not expose its identity")
        .to_owned();
    let mut codex = fixture.connect().await;
    let mut claude = fixture.connect().await;
    hello(&mut codex).await;
    hello(&mut claude).await;
    let codex_open = request(
        &mut codex,
        "open-codex-for-universal",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let claude_open = request(
        &mut claude,
        "open-claude-for-universal",
        json!({ "kind": "open-target", "target": "claude" }),
    )
    .await;
    assert_eq!(codex_open["result"]["view"]["viewSequence"], 0);
    assert_eq!(claude_open["result"]["view"]["viewSequence"], 0);

    write_frame(
        &mut initiator,
        &json!({
            "type": "request",
            "requestId": "synchronize-universal",
            "operation": {
                "kind": "universal-provider-act",
                "actionId": "00000000-0000-4000-8000-000000000902",
                "expectedRevision": 1,
                "action": {
                    "kind": "synchronize-universal-provider",
                    "providerId": provider_id,
                    "providerRevision": 1
                }
            }
        }),
    )
    .await
    .unwrap();
    let synchronized = read_frame(&mut initiator).await.unwrap();
    assert_eq!(
        synchronized["result"]["outcome"]["status"].as_str(),
        Some("applied"),
        "cross-Target synchronization did not return an applied catalog outcome",
    );
    let initiating_catalog_push = read_frame(&mut initiator).await.unwrap();
    let subscribing_catalog_push = read_frame(&mut subscriber).await.unwrap();
    let codex_push = read_frame(&mut codex).await.unwrap();
    let claude_push = read_frame(&mut claude).await.unwrap();
    for push in [&initiating_catalog_push, &subscribing_catalog_push] {
        assert_eq!(push["type"].as_str(), Some("universal-provider-view"));
        assert_eq!(push["view"]["revision"].as_u64(), Some(2));
        assert_eq!(
            push["view"]["providers"][0]["targets"][0]["synchronization"].as_str(),
            Some("current"),
        );
        assert_eq!(
            push["view"]["providers"][0]["targets"][1]["synchronization"].as_str(),
            Some("current"),
        );
    }
    assert_eq!(codex_push["type"].as_str(), Some("target-view"));
    assert_eq!(codex_push["view"]["target"].as_str(), Some("codex"));
    assert_eq!(codex_push["view"]["providers"][0]["generated"], true);
    assert_eq!(claude_push["type"].as_str(), Some("target-view"));
    assert_eq!(claude_push["view"]["target"].as_str(), Some("claude"));
    assert_eq!(claude_push["view"]["providers"][0]["generated"], true);

    write_frame(
        &mut initiator,
        &json!({
            "type": "request",
            "requestId": "update-synchronized-universal",
            "operation": {
                "kind": "universal-provider-act",
                "actionId": "00000000-0000-4000-8000-000000000904",
                "expectedRevision": 2,
                "action": {
                    "kind": "update-universal-provider",
                    "providerId": provider_id,
                    "providerRevision": 1,
                    "name": "Shared Gateway Updated",
                    "baseUrl": "https://universal-session-updated.example/v1",
                    "credential": { "kind": "keep" },
                    "targets": [
                        {
                            "target": "codex",
                            "enabled": true,
                            "model": "shared-model",
                            "authentication": "openai-bearer",
                            "routingRequirement": "direct-compatible"
                        },
                        {
                            "target": "claude",
                            "enabled": true,
                            "model": "shared-model",
                            "authentication": "anthropic-bearer",
                            "routingRequirement": "takeover-required"
                        }
                    ]
                }
            }
        }),
    )
    .await
    .unwrap();
    let updated = read_frame(&mut initiator).await.unwrap();
    assert_eq!(updated["result"]["outcome"]["view"]["revision"], 3);
    let _initiating_update_push = read_frame(&mut initiator).await.unwrap();
    let _subscribing_update_push = read_frame(&mut subscriber).await.unwrap();
    let codex_pending = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut codex))
        .await
        .expect("Universal source edit did not publish the affected Codex Target View")
        .unwrap();
    let claude_pending = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut claude))
        .await
        .expect("Universal source edit did not publish the affected Claude Target View")
        .unwrap();
    for (push, target) in [(&codex_pending, "codex"), (&claude_pending, "claude")] {
        assert_eq!(push["type"].as_str(), Some("target-view"));
        assert_eq!(push["view"]["target"].as_str(), Some(target));
        assert_eq!(push["view"]["providers"][0]["synchronization"], "pending");
    }

    let stale = request(
        &mut initiator,
        "stale-universal",
        json!({
            "kind": "universal-provider-act",
            "actionId": "00000000-0000-4000-8000-000000000903",
            "expectedRevision": 2,
            "action": {
                "kind": "delete-universal-provider",
                "providerId": provider_id,
                "providerRevision": 1
            }
        }),
    )
    .await;
    assert_eq!(
        stale["problem"]["code"].as_str(),
        Some("stale-universal-catalog-revision"),
        "stale catalog revision returned the wrong fixed problem",
    );
    let refreshed = request(
        &mut initiator,
        "refresh-universal-after-stale",
        json!({ "kind": "open-universal-providers" }),
    )
    .await;
    assert_eq!(
        refreshed["result"]["view"]["revision"].as_u64(),
        Some(3),
        "catalog refresh after a stale action did not return authoritative state",
    );
}

#[tokio::test]
async fn generated_overlay_update_publishes_the_authoritative_universal_catalog() {
    let fixture = ControlFixture::start().await;
    let mut catalog = fixture.connect().await;
    let mut codex = fixture.connect().await;
    hello(&mut catalog).await;
    hello(&mut codex).await;
    let _opened_catalog = request(
        &mut catalog,
        "open-generated-overlay-catalog",
        json!({ "kind": "open-universal-providers" }),
    )
    .await;
    let _opened_codex = request(
        &mut codex,
        "open-generated-overlay-codex",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    let created = request(
        &mut catalog,
        "create-generated-overlay-source",
        json!({
            "kind": "universal-provider-act",
            "actionId": "00000000-0000-4000-8000-000000000941",
            "expectedRevision": 0,
            "action": {
                "kind": "create-universal-provider",
                "name": "Overlay Source",
                "baseUrl": "https://overlay-source.example/v1",
                "credential": { "kind": "replace", "value": "OVERLAY_SOURCE_SECRET_941" },
                "presetKey": null,
                "targets": [
                    { "target": "codex", "enabled": true, "model": "overlay-v1", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "unused", "authentication": "anthropic-api-key", "routingRequirement": "direct-compatible" }
                ]
            }
        }),
    )
    .await;
    let _created_push = read_frame(&mut catalog).await.unwrap();
    let source_id = created["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let synchronized = request(
        &mut catalog,
        "sync-generated-overlay-source",
        json!({
            "kind": "universal-provider-act",
            "actionId": "00000000-0000-4000-8000-000000000942",
            "expectedRevision": 1,
            "action": {
                "kind": "synchronize-universal-provider",
                "providerId": source_id,
                "providerRevision": 1
            }
        }),
    )
    .await;
    let _synchronized_catalog_push = read_frame(&mut catalog).await.unwrap();
    let generated_target_push = read_frame(&mut codex).await.unwrap();
    let generated = &synchronized["result"]["outcome"]["view"]["providers"][0]["targets"][0];
    let generated_id = generated["generatedProviderId"].as_str().unwrap();
    let target_revision = generated_target_push["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let provider_revision = generated_target_push["view"]["providers"][0]["providerRevision"]
        .as_u64()
        .unwrap();

    write_frame(
        &mut codex,
        &json!({
            "type": "request",
            "requestId": "update-generated-overlay",
            "operation": {
                "kind": "act",
                "target": "codex",
                "actionId": "00000000-0000-4000-8000-000000000943",
                "expectedRevision": target_revision,
                "action": {
                    "kind": "update-provider",
                    "providerId": generated_id,
                    "providerRevision": provider_revision,
                    "name": "Overlay Source",
                    "baseUrl": "https://overlay-source.example/v1",
                    "model": "overlay-v2",
                    "authentication": "openai-bearer",
                    "routingRequirement": "takeover-required",
                    "credential": { "kind": "keep" }
                }
            }
        }),
    )
    .await
    .unwrap();
    let response = read_frame(&mut codex).await.unwrap();
    assert_eq!(response["type"], "response");
    let _target_push = read_frame(&mut codex).await.unwrap();
    let catalog_push = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut catalog))
        .await
        .expect("Generated Overlay update did not publish the Universal Provider catalog")
        .unwrap();
    assert_eq!(catalog_push["type"], "universal-provider-view");
    assert_eq!(catalog_push["view"]["revision"], 3);
    assert_eq!(
        catalog_push["view"]["providers"][0]["targets"][0]["model"],
        "overlay-v2"
    );
    assert_eq!(
        catalog_push["view"]["providers"][0]["targets"][0]["routingRequirement"],
        "takeover-required"
    );
    assert!(
        !catalog_push
            .to_string()
            .contains("OVERLAY_SOURCE_SECRET_941")
    );
}

#[tokio::test]
async fn generated_activation_advances_catalog_references_and_disable_returns_authoritative_catalog()
 {
    const SECRETS: &[&str] = &["REFERENCE_SOURCE_SECRET_951"];
    let fixture = ControlFixture::start().await;
    let mut catalog = fixture.connect().await;
    let mut codex = fixture.connect().await;
    hello(&mut catalog).await;
    hello(&mut codex).await;
    let _opened_catalog = request(
        &mut catalog,
        "open-generated-reference-catalog",
        json!({ "kind": "open-universal-providers" }),
    )
    .await;
    let opened_codex = request(
        &mut codex,
        "open-generated-reference-codex",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    let created = request(
        &mut catalog,
        "create-generated-reference-source",
        json!({
            "kind": "universal-provider-act",
            "actionId": "00000000-0000-4000-8000-000000000951",
            "expectedRevision": 0,
            "action": {
                "kind": "create-universal-provider",
                "name": "Reference Source",
                "baseUrl": "https://reference-source.example/v1",
                "credential": { "kind": "replace", "value": "REFERENCE_SOURCE_SECRET_951" },
                "presetKey": null,
                "targets": [
                    { "target": "codex", "enabled": true, "model": "reference-model", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "unused", "authentication": "anthropic-api-key", "routingRequirement": "direct-compatible" }
                ]
            }
        }),
    )
    .await;
    assert_compatibility_wire_is_secret_free(&created, SECRETS, "generated-reference-create");
    let _created_catalog_push = read_frame(&mut catalog).await.unwrap();
    let source_id = created["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let synchronized = request(
        &mut catalog,
        "sync-generated-reference-source",
        json!({
            "kind": "universal-provider-act",
            "actionId": "00000000-0000-4000-8000-000000000952",
            "expectedRevision": 1,
            "action": {
                "kind": "synchronize-universal-provider",
                "providerId": source_id,
                "providerRevision": 1
            }
        }),
    )
    .await;
    assert_compatibility_wire_is_secret_free(&synchronized, SECRETS, "generated-reference-sync");
    let _synchronized_catalog_push = read_frame(&mut catalog).await.unwrap();
    let generated_target_push = read_frame(&mut codex).await.unwrap();
    assert_compatibility_wire_is_secret_free(
        &generated_target_push,
        SECRETS,
        "generated-reference-target-push",
    );
    let generated_id = synchronized["result"]["outcome"]["view"]["providers"][0]["targets"][0]
        ["generatedProviderId"]
        .as_str()
        .unwrap();
    let catalog_sequence = synchronized["result"]["outcome"]["view"]["viewSequence"]
        .as_u64()
        .unwrap();
    let target_revision = generated_target_push["view"]["managementRevision"]
        .as_u64()
        .unwrap_or_else(|| {
            opened_codex["result"]["view"]["managementRevision"]
                .as_u64()
                .unwrap()
        });

    let activated = request(
        &mut codex,
        "activate-generated-reference",
        json!({
            "kind": "act",
            "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000953",
            "expectedRevision": target_revision,
            "action": {
                "kind": "activate-provider",
                "providerId": generated_id,
                "mode": "direct"
            }
        }),
    )
    .await;
    assert_compatibility_wire_is_secret_free(&activated, SECRETS, "generated-reference-activation");
    assert_eq!(activated["type"], "response");
    let activated_target_push = read_frame(&mut codex).await.unwrap();
    assert_compatibility_wire_is_secret_free(
        &activated_target_push,
        SECRETS,
        "generated-reference-activation-push",
    );
    let catalog_push = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut catalog))
        .await
        .expect("Generated activation did not publish updated catalog references")
        .unwrap();
    assert_compatibility_wire_is_secret_free(
        &catalog_push,
        SECRETS,
        "generated-reference-catalog-push",
    );
    assert_eq!(catalog_push["type"], "universal-provider-view");
    assert!(catalog_push["view"]["viewSequence"].as_u64().unwrap() > catalog_sequence);
    assert_eq!(
        catalog_push["view"]["providers"][0]["targets"][0]["activeReferences"],
        json!(["current", "activated-snapshot", "activated-route-plan"])
    );

    let blocked_disable = request(
        &mut catalog,
        "disable-generated-reference",
        json!({
            "kind": "universal-provider-act",
            "actionId": "00000000-0000-4000-8000-000000000954",
            "expectedRevision": 2,
            "action": {
                "kind": "update-universal-provider",
                "providerId": source_id,
                "providerRevision": 1,
                "name": "Reference Source",
                "baseUrl": "https://reference-source.example/v1",
                "credential": { "kind": "keep" },
                "targets": [
                    { "target": "codex", "enabled": false, "model": "reference-model", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "unused", "authentication": "anthropic-api-key", "routingRequirement": "direct-compatible" }
                ]
            }
        }),
    )
    .await;
    assert_compatibility_wire_is_secret_free(
        &blocked_disable,
        SECRETS,
        "generated-reference-disable",
    );
    assert_eq!(blocked_disable["type"], "error");
    assert_eq!(
        blocked_disable["problem"]["code"],
        "generated-provider-referenced"
    );
    assert_eq!(
        blocked_disable["authoritativeUniversalProviderView"]["providers"][0]["targets"][0]["activeReferences"],
        json!(["current", "activated-snapshot", "activated-route-plan"])
    );
    let restore_preview = request(
        &mut codex,
        "preview-generated-reference-restore",
        json!({
            "kind": "preview-reconciliation",
            "target": "codex",
            "strategy": "restore"
        }),
    )
    .await;
    assert_compatibility_wire_is_secret_free(
        &restore_preview,
        SECRETS,
        "generated-reference-restore-preview",
    );
    let restored = request(
        &mut codex,
        "restore-generated-reference",
        json!({
            "kind": "act",
            "target": "codex",
            "actionId": "00000000-0000-4000-8000-000000000955",
            "expectedRevision": activated["result"]["outcome"]["view"]["managementRevision"],
            "action": {
                "kind": "reconcile",
                "strategy": "restore",
                "observationToken": restore_preview["result"]["preview"]["observationToken"]
            }
        }),
    )
    .await;
    assert_compatibility_wire_is_secret_free(&restored, SECRETS, "generated-reference-restore");
    assert_eq!(restored["type"], "response", "{restored:?}");
    let restored_target_push = read_frame(&mut codex).await.unwrap();
    assert_compatibility_wire_is_secret_free(
        &restored_target_push,
        SECRETS,
        "generated-reference-restore-target-push",
    );
    let restored_catalog_push =
        tokio::time::timeout(Duration::from_secs(1), read_frame(&mut catalog))
            .await
            .expect("Generated Restore did not publish updated catalog references")
            .unwrap();
    assert_compatibility_wire_is_secret_free(
        &restored_catalog_push,
        SECRETS,
        "generated-reference-restore-catalog-push",
    );
    assert_eq!(restored_catalog_push["type"], "universal-provider-view");
    assert_eq!(
        restored_catalog_push["view"]["providers"][0]["targets"][0]["activeReferences"],
        json!([])
    );
    assert!(
        restored_catalog_push["view"]["viewSequence"]
            .as_u64()
            .unwrap()
            > catalog_push["view"]["viewSequence"].as_u64().unwrap()
    );

    let disabled = request(
        &mut catalog,
        "disable-restored-generated-reference",
        json!({
            "kind": "universal-provider-act",
            "actionId": "00000000-0000-4000-8000-000000000956",
            "expectedRevision": 2,
            "action": {
                "kind": "update-universal-provider",
                "providerId": source_id,
                "providerRevision": 1,
                "name": "Reference Source",
                "baseUrl": "https://reference-source.example/v1",
                "credential": { "kind": "keep" },
                "targets": [
                    { "target": "codex", "enabled": false, "model": "reference-model", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "unused", "authentication": "anthropic-api-key", "routingRequirement": "direct-compatible" }
                ]
            }
        }),
    )
    .await;
    assert_compatibility_wire_is_secret_free(
        &disabled,
        SECRETS,
        "generated-reference-disable-after-restore",
    );
    assert_eq!(disabled["type"], "response");
}

#[tokio::test]
async fn catalog_delete_waits_for_target_activation_and_observes_the_committed_reference() {
    const SECRETS: &[&str] = &["DELETE_RACE_SECRET_961"];
    let pause = Arc::new(ActivationPause::default());
    let fixture = ControlFixture::start_with_activation_hooks(
        ActivationHooks::pausing_final_commit(Arc::clone(&pause)),
    )
    .await;
    let mut catalog = fixture.connect().await;
    hello(&mut catalog).await;
    let _opened_catalog = request(
        &mut catalog,
        "open-delete-race-catalog",
        json!({ "kind": "open-universal-providers" }),
    )
    .await;

    let created = request(
        &mut catalog,
        "create-delete-race-source",
        json!({
            "kind": "universal-provider-act",
            "actionId": "00000000-0000-4000-8000-000000000961",
            "expectedRevision": 0,
            "action": {
                "kind": "create-universal-provider",
                "name": "Delete Race Source",
                "baseUrl": "https://delete-race.example/v1",
                "credential": { "kind": "replace", "value": "DELETE_RACE_SECRET_961" },
                "presetKey": null,
                "targets": [
                    { "target": "codex", "enabled": true, "model": "delete-race-model", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "unused", "authentication": "anthropic-api-key", "routingRequirement": "direct-compatible" }
                ]
            }
        }),
    )
    .await;
    assert_compatibility_wire_is_secret_free(&created, SECRETS, "delete-race-create");
    let created_push = read_frame(&mut catalog).await.unwrap();
    assert_compatibility_wire_is_secret_free(&created_push, SECRETS, "delete-race-create-push");
    let source_id = created["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let synchronized = request(
        &mut catalog,
        "sync-delete-race-source",
        json!({
            "kind": "universal-provider-act",
            "actionId": "00000000-0000-4000-8000-000000000962",
            "expectedRevision": 1,
            "action": {
                "kind": "synchronize-universal-provider",
                "providerId": source_id,
                "providerRevision": 1
            }
        }),
    )
    .await;
    assert_compatibility_wire_is_secret_free(&synchronized, SECRETS, "delete-race-sync");
    let synchronized_push = read_frame(&mut catalog).await.unwrap();
    assert_compatibility_wire_is_secret_free(&synchronized_push, SECRETS, "delete-race-sync-push");
    let generated_id = synchronized["result"]["outcome"]["view"]["providers"][0]["targets"]
        [0]["generatedProviderId"]
        .as_str()
        .unwrap();
    let mut codex = fixture.connect().await;
    hello(&mut codex).await;
    let opened_codex = request(
        &mut codex,
        "open-delete-race-codex",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let target_revision = opened_codex["result"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();

    write_frame(
        &mut codex,
        &json!({
            "type": "request",
            "requestId": "activate-delete-race",
            "operation": {
                "kind": "act",
                "target": "codex",
                "actionId": "00000000-0000-4000-8000-000000000963",
                "expectedRevision": target_revision,
                "action": {
                    "kind": "activate-provider",
                    "providerId": generated_id,
                    "mode": "direct"
                }
            }
        }),
    )
    .await
    .unwrap();
    pause.wait_until_reached().await;

    write_frame(
        &mut catalog,
        &json!({
            "type": "request",
            "requestId": "delete-racing-source",
            "operation": {
                "kind": "universal-provider-act",
                "actionId": "00000000-0000-4000-8000-000000000964",
                "expectedRevision": 2,
                "action": {
                    "kind": "delete-universal-provider",
                    "providerId": source_id,
                    "providerRevision": 1
                }
            }
        }),
    )
    .await
    .unwrap();
    let premature =
        tokio::time::timeout(Duration::from_millis(150), read_frame(&mut catalog)).await;
    pause.release();
    assert!(
        premature.is_err(),
        "catalog delete crossed an in-flight Target activation"
    );

    let activated = read_frame(&mut codex).await.unwrap();
    assert_compatibility_wire_is_secret_free(&activated, SECRETS, "delete-race-activation");
    assert_eq!(activated["type"], "response");
    let target_push = read_frame(&mut codex).await.unwrap();
    assert_compatibility_wire_is_secret_free(&target_push, SECRETS, "delete-race-target-push");
    let mut blocked = None;
    for _ in 0..3 {
        let frame = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut catalog))
            .await
            .unwrap()
            .unwrap();
        if frame["type"] == "error" {
            blocked = Some(frame);
            break;
        }
    }
    let blocked = blocked.expect("catalog delete did not return a reference failure");
    assert_compatibility_wire_is_secret_free(&blocked, SECRETS, "delete-race-blocked");
    assert_eq!(blocked["problem"]["code"], "generated-provider-referenced");
    assert_eq!(
        blocked["authoritativeUniversalProviderView"]["providers"][0]["targets"][0]["activeReferences"],
        json!(["current", "activated-snapshot", "activated-route-plan"])
    );
}

#[tokio::test]
async fn universal_provider_writer_failure_suppresses_push_and_reconnects_to_durable_catalog() {
    let fixture = ControlFixture::start().await;
    let created = fixture
        .store
        .apply_universal_provider_action(
            Uuid::from_u128(0x910),
            0,
            json!({
                "kind": "create-universal-provider",
                "name": "Inflated catalog",
                "baseUrl": "https://writer-failure.example/v1",
                "credential": { "kind": "remove" },
                "presetKey": null,
                "targets": [
                    {
                        "target": "codex",
                        "enabled": true,
                        "model": "m".repeat(128 * 1024),
                        "authentication": "openai-bearer",
                        "routingRequirement": "direct-compatible"
                    },
                    {
                        "target": "claude",
                        "enabled": false,
                        "model": "m".repeat(128 * 1024),
                        "authentication": "anthropic-api-key",
                        "routingRequirement": "direct-compatible"
                    }
                ]
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;

    let mut initiator = fixture.connect().await;
    let mut subscriber = fixture.connect().await;
    hello(&mut initiator).await;
    hello(&mut subscriber).await;
    request(
        &mut initiator,
        "open-writer-failure-initiator",
        json!({ "kind": "open-universal-providers" }),
    )
    .await;
    request(
        &mut subscriber,
        "open-writer-failure-subscriber",
        json!({ "kind": "open-universal-providers" }),
    )
    .await;

    for index in 0..8 {
        write_frame(
            &mut initiator,
            &json!({
                "type": "request",
                "requestId": format!("universal-fill-{index}"),
                "operation": { "kind": "open-universal-providers" }
            }),
        )
        .await
        .unwrap();
    }
    write_frame(
        &mut initiator,
        &json!({
            "type": "request",
            "requestId": "universal-writer-failure-action",
            "operation": {
                "kind": "universal-provider-act",
                "actionId": "00000000-0000-4000-8000-000000000911",
                "expectedRevision": 1,
                "action": {
                    "kind": "update-universal-provider",
                    "providerId": provider_id,
                    "providerRevision": 1,
                    "name": "Durable after writer failure",
                    "baseUrl": "https://writer-failure.example/v1",
                    "credential": { "kind": "keep" },
                    "targets": [
                        {
                            "target": "codex",
                            "enabled": true,
                            "model": "m".repeat(128 * 1024),
                            "authentication": "openai-bearer",
                            "routingRequirement": "direct-compatible"
                        },
                        {
                            "target": "claude",
                            "enabled": false,
                            "model": "m".repeat(128 * 1024),
                            "authentication": "anthropic-api-key",
                            "routingRequirement": "direct-compatible"
                        }
                    ]
                }
            }
        }),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            if fixture
                .store
                .universal_provider_catalog()
                .await
                .unwrap()
                .revision
                == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Universal Provider action did not durably commit behind the blocked writer");
    assert!(
        tokio::time::timeout(Duration::from_millis(40), read_frame(&mut subscriber))
            .await
            .is_err(),
        "subscriber received a catalog push before the initiating response writer ack",
    );
    drop(initiator);
    assert!(
        tokio::time::timeout(Duration::from_millis(320), read_frame(&mut subscriber))
            .await
            .is_err(),
        "writer failure published a catalog view",
    );

    let mut reconnected = fixture.connect().await;
    hello(&mut reconnected).await;
    let visible = request(
        &mut reconnected,
        "open-after-universal-writer-failure",
        json!({ "kind": "open-universal-providers" }),
    )
    .await;
    assert_eq!(visible["result"]["view"]["revision"].as_u64(), Some(2));
    assert_eq!(
        visible["result"]["view"]["providers"][0]["name"].as_str(),
        Some("Durable after writer failure"),
    );
}

async fn seed_codex_direct(home: &MuxviaHome, store: Arc<StateStore>) {
    let created = store
        .apply_provider_action_for(
            muxvia_routing::control::protocol::Target::Codex,
            Uuid::new_v4(),
            0,
            json!({
                "kind": "create-provider",
                "name": "Codex",
                "baseUrl": "https://seed.test/v1",
                "model": "seed-model",
                "credential": { "kind": "replace", "value": "seed-secret" },
                "authentication": "openai-bearer",
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    let activation = ActivationService::new(
        Arc::clone(&store),
        home.clone(),
        Arc::new(ControlCodexProbe),
        "/usr/bin/codex".into(),
        Arc::new(ControlNoopUpstream),
    );
    activation
        .apply_raw_for(
            muxvia_routing::control::protocol::Target::Codex,
            Uuid::new_v4(),
            created.view.management_revision,
            json!({
                "kind": "activate-provider",
                "providerId": created.view.providers[0].id,
                "mode": "direct"
            }),
        )
        .await
        .unwrap();
}

async fn inflate_codex_target_view(store: &Arc<StateStore>) {
    let revision = store
        .target_view_for(Target::Codex)
        .await
        .unwrap()
        .management_revision;
    store
        .apply_provider_action_for(
            Target::Codex,
            Uuid::new_v4(),
            revision,
            json!({
                "kind": "create-provider",
                "name": "Writer backpressure",
                "baseUrl": "https://writer-backpressure.test/v1",
                "model": "m".repeat(128 * 1024),
                "credential": { "kind": "replace", "value": "writer-backpressure-secret" },
                "authentication": "openai-bearer",
                "presetKey": null
            }),
        )
        .await
        .unwrap();
}

async fn queue_writer_backpressure(stream: &mut UnixStream, prefix: &str) {
    for index in 0..8 {
        write_frame(
            stream,
            &json!({
                "type": "request",
                "requestId": format!("{prefix}-{index}"),
                "operation": { "kind": "open-target", "target": "codex" }
            }),
        )
        .await
        .unwrap();
    }
}

async fn seed_claude_direct(home: &MuxviaHome, store: Arc<StateStore>) {
    let created = store
        .apply_provider_action_for(
            Target::Claude,
            Uuid::new_v4(),
            0,
            json!({
                "kind": "create-provider",
                "name": "Claude",
                "baseUrl": "https://seed.anthropic.test",
                "model": "seed-claude",
                "credential": { "kind": "replace", "value": "seed-claude-secret" },
                "authentication": "anthropic-api-key",
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    let context = ClaudePreflightContext {
        claude_config_dir: None,
        selector_state: ClaudeSelectorState::Unset,
        blocking_selector: None,
        host_managed_state: ClaudeHostManagedState::Unmanaged,
        cwd: home.user_home().to_string_lossy().into_owned(),
    };
    let activation = ActivationService::new(
        Arc::clone(&store),
        home.clone(),
        Arc::new(ControlCodexProbe),
        "/usr/bin/codex".into(),
        Arc::new(ControlNoopUpstream),
    )
    .with_claude_runtime(Arc::new(ControlClaudeProbe), "/usr/bin/claude".into());
    activation
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            created.view.management_revision,
            json!({
                "kind": "activate-provider",
                "providerId": created.view.providers[0].id,
                "mode": "direct"
            }),
            Some(&context),
        )
        .await
        .unwrap();
}

fn hanging_probe_executable(root: &Path) -> (PathBuf, PathBuf) {
    let started = root.join("probe-started");
    let pid = root.join("probe.pid");
    let executable = root.join("hanging-codex");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\ntouch '{}'\nprintf 'HUNG_PROBE_STDERR_SECRET_92017\\n' >&2\nexec /bin/sleep 2\n",
            pid.display(),
            started.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    (executable, started)
}

async fn wait_for_path(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn process_is_reaped(pid_path: &Path) -> bool {
    let pid: i32 = fs::read_to_string(pid_path).unwrap().parse().unwrap();
    pid_is_reaped(pid)
}

fn pid_is_reaped(pid: i32) -> bool {
    // SAFETY: signal 0 only checks whether the recorded child PID still exists.
    (unsafe { libc::kill(pid, 0) }) == -1
}

fn completing_probe_executable(root: &Path) -> (PathBuf, PathBuf) {
    let pids = root.join("completed-probe.pids");
    let executable = root.join("completing-codex");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> '{}'\ncase \"$1\" in\n --version) printf 'codex-cli 0.147.0\\n' ;;\n --help) printf 'Usage: codex [OPTIONS]\\n--config <key=value>\\n' ;;\n *) exit 91 ;;\nesac\n",
            pids.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    (executable, pids)
}

fn completing_claude_probe_executable(root: &Path) -> PathBuf {
    let executable = root.join("completing-claude");
    fs::write(
        &executable,
        "#!/bin/sh\ncase \"$1\" in\n --version) printf '2.1.228 (Claude Code)\\n' ;;\n --help) printf 'Usage: claude [options] [command]\\n--settings <file>\\n--model <model>\\n' ;;\n *) exit 91 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

fn exited_probe_with_inherited_stdout(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let descendant_pid = root.join("probe-descendant.pid");
    let parent_pid = root.join("probe-parent.pid");
    let executable = root.join("exited-probe-with-open-stdout");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\ncase \"$1\" in\n --version) (/bin/sleep 2) & printf '%s' \"$!\" > '{}'; printf 'codex-cli 0.147.0\\n' ;;\n --help) printf 'Usage: codex [OPTIONS]\\n--config <key=value>\\n' ;;\n *) exit 91 ;;\nesac\n",
            parent_pid.display(),
            descendant_pid.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    (executable, descendant_pid, parent_pid)
}

fn exited_claude_probe_with_inherited_stdout(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let descendant_pid = root.join("claude-probe-descendant.pid");
    let parent_pid = root.join("claude-probe-parent.pid");
    let executable = root.join("exited-claude-probe-with-open-stdout");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\ncase \"$1\" in\n --version) (/bin/sleep 2) & printf '%s' \"$!\" > '{}'; printf '2.1.228 (Claude Code)\\n' ;;\n --help) printf 'Usage: claude [options] [command]\\n--settings <file>\\n--model <model>\\n' ;;\n *) exit 91 ;;\nesac\n",
            parent_pid.display(),
            descendant_pid.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    (executable, descendant_pid, parent_pid)
}

async fn start_hanging_preview_fixture(
    prefix: &str,
) -> (PathBuf, ControlServerHandle, UnixStream, PathBuf) {
    let root = short_temp_root(prefix);
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    seed_codex_direct(&home, Arc::clone(&store)).await;
    let (executable, started) = hanging_probe_executable(&root);
    let activation = Arc::new(ActivationService::new(
        Arc::clone(&store),
        home,
        Arc::new(CommandCodexProbe),
        executable,
        Arc::new(ControlNoopUpstream),
    ));
    let handle = ControlServer::bind_with_activation(
        &MuxviaHome::from_user_home(&user_home),
        store,
        "routing-test",
        activation,
    )
    .await
    .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let _opened = request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "hanging-preview",
            "operation": {
                "kind": "preview-reconciliation",
                "target": "codex",
                "strategy": "reapply"
            }
        }),
    )
    .await
    .unwrap();
    wait_for_path(&started).await;
    let pid = root.join("probe.pid");
    (root, handle, stream, pid)
}

async fn start_custom_blocking_preview_fixture(
    prefix: &str,
) -> (
    PathBuf,
    ControlServerHandle,
    UnixStream,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let root = short_temp_root(prefix);
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    seed_codex_direct(&home, Arc::clone(&store)).await;
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let activation = Arc::new(ActivationService::new(
        Arc::clone(&store),
        home.clone(),
        Arc::new(BlockingFallbackCodexProbe {
            started: started_tx,
            release: std::sync::Mutex::new(release_rx),
        }),
        "/custom/blocking-probe".into(),
        Arc::new(ControlNoopUpstream),
    ));
    let handle = ControlServer::bind_with_activation(&home, store, "routing-test", activation)
        .await
        .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let _opened = request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "blocking-custom-preview",
            "operation": {
                "kind": "preview-reconciliation",
                "target": "codex",
                "strategy": "reapply"
            }
        }),
    )
    .await
    .unwrap();
    (root, handle, stream, started_rx, release_tx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciliation_preview_disconnect_cancels_custom_probe_without_sync_fallback() {
    let (root, handle, stream, started_rx, release_tx) =
        start_custom_blocking_preview_fixture("mx-preview-custom-cancel").await;
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(handle.tracked_inspections(), 1);
    drop(stream);
    let cancelled_before_release = tokio::time::timeout(Duration::from_millis(300), async {
        while handle.tracked_inspections() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let _ = release_tx.send(());
    tokio::time::timeout(Duration::from_secs(1), async {
        while handle.tracked_inspections() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
    assert!(
        cancelled_before_release,
        "preview used the noncancellable synchronous probe fallback"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciliation_preview_shutdown_cancels_custom_probe_without_sync_fallback() {
    let (root, handle, _stream, started_rx, release_tx) =
        start_custom_blocking_preview_fixture("mx-preview-custom-stop").await;
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(handle.tracked_inspections(), 1);
    let mut shutdown = Box::pin(handle.shutdown());
    let shutdown_before_release = tokio::time::timeout(Duration::from_millis(300), &mut shutdown)
        .await
        .is_ok();
    let _ = release_tx.send(());
    if !shutdown_before_release {
        tokio::time::timeout(Duration::from_secs(1), &mut shutdown)
            .await
            .unwrap()
            .unwrap();
    }
    let _ = fs::remove_dir_all(root);
    assert!(
        shutdown_before_release,
        "shutdown used the noncancellable synchronous probe fallback"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciliation_preview_disconnect_kills_hung_probe_and_reaps_tracking() {
    let (root, handle, stream, pid) = start_hanging_preview_fixture("mx-preview-drop").await;
    assert_eq!(handle.tracked_inspections(), 1);
    drop(stream);
    tokio::time::timeout(Duration::from_millis(500), async {
        while handle.tracked_inspections() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect did not reap hanging preview");
    let reaped_before_tracking_completed = process_is_reaped(&pid);
    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
    assert!(
        reaped_before_tracking_completed,
        "tracked inspection completed before the probe child was reaped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciliation_preview_shutdown_kills_hung_probe_and_is_bounded() {
    let (root, handle, _stream, pid) = start_hanging_preview_fixture("mx-preview-stop").await;
    assert_eq!(handle.tracked_inspections(), 1);
    tokio::time::timeout(Duration::from_millis(500), handle.shutdown())
        .await
        .expect("shutdown was starved by hanging preview")
        .unwrap();
    let reaped_before_shutdown_returned = process_is_reaped(&pid);
    let _ = fs::remove_dir_all(root);
    assert!(
        reaped_before_shutdown_returned,
        "shutdown returned before the probe child was reaped"
    );
}

#[tokio::test]
async fn reconciliation_preview_normal_completion_reaps_every_probe_child_before_response() {
    let root = short_temp_root("mx-preview-complete");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    seed_codex_direct(&home, Arc::clone(&store)).await;
    let (executable, pids_path) = completing_probe_executable(&root);
    let activation = Arc::new(ActivationService::new(
        Arc::clone(&store),
        home.clone(),
        Arc::new(CommandCodexProbe),
        executable,
        Arc::new(ControlNoopUpstream),
    ));
    let handle = ControlServer::bind_with_activation(&home, store, "routing-test", activation)
        .await
        .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let _opened = request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let preview = request(
        &mut stream,
        "normal-preview",
        json!({
            "kind": "preview-reconciliation",
            "target": "codex",
            "strategy": "reapply"
        }),
    )
    .await;
    assert_eq!(preview["type"], "response");
    assert_eq!(
        preview["result"]["preview"]["compatibility"],
        json!({
            "version": "codex-cli 0.147.0",
            "classification": "tested",
            "acknowledgementRequired": false
        })
    );
    assert_eq!(handle.tracked_inspections(), 0);
    let pids = fs::read_to_string(pids_path)
        .unwrap()
        .lines()
        .map(|pid| pid.parse::<i32>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 2);
    assert!(
        pids.into_iter().all(pid_is_reaped),
        "normal response was written before every probe child was reaped"
    );
    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn reconciliation_preview_claude_command_probe_projects_exact_success_over_uds() {
    let root = short_temp_root("mx-preview-claude-command");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    seed_claude_direct(&home, Arc::clone(&store)).await;
    let executable = completing_claude_probe_executable(&root);
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(ControlCodexProbe),
            "/usr/bin/codex".into(),
            Arc::new(ControlNoopUpstream),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), executable),
    );
    let handle = ControlServer::bind_with_activation(&home, store, "routing-test", activation)
        .await
        .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let _ = request(
        &mut stream,
        "open",
        json!({
            "kind": "open-target",
            "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "unset",
                "hostManagedState": "unmanaged",
                "cwd": user_home
            }
        }),
    )
    .await;
    let preview = request(
        &mut stream,
        "claude-command-preview",
        json!({
            "kind": "preview-reconciliation",
            "target": "claude",
            "strategy": "reapply"
        }),
    )
    .await;
    assert_eq!(
        preview["result"]["preview"]["compatibility"],
        json!({
            "version": "2.1.228 (Claude Code)",
            "classification": "tested",
            "acknowledgementRequired": false
        })
    );
    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciliation_preview_disconnect_cancels_stdout_reader_after_probe_exit() {
    let root = short_temp_root("mx-preview-open-stdout");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    seed_codex_direct(&home, Arc::clone(&store)).await;
    let (executable, descendant_pid, parent_pid) = exited_probe_with_inherited_stdout(&root);
    let activation = Arc::new(ActivationService::new(
        Arc::clone(&store),
        home.clone(),
        Arc::new(CommandCodexProbe),
        executable,
        Arc::new(ControlNoopUpstream),
    ));
    let handle = ControlServer::bind_with_activation(&home, store, "routing-test", activation)
        .await
        .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let _ = request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "open-stdout-preview",
            "operation": {
                "kind": "preview-reconciliation",
                "target": "codex",
                "strategy": "reapply"
            }
        }),
    )
    .await
    .unwrap();
    wait_for_path(&descendant_pid).await;
    wait_for_path(&parent_pid).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !process_is_reaped(&parent_pid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("probe parent did not exit before disconnect");
    assert_eq!(handle.tracked_inspections(), 1);
    drop(stream);
    tokio::time::timeout(Duration::from_millis(500), async {
        while handle.tracked_inspections() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect did not close the inherited stdout reader");
    handle.shutdown().await.unwrap();
    let descendant_pid = fs::read_to_string(&descendant_pid)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    // SAFETY: the PID was written by the controlled descendant in this test fixture.
    let _ = unsafe { libc::kill(descendant_pid, libc::SIGTERM) };
    let _ = fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciliation_preview_disconnect_cancels_claude_stdout_reader_after_probe_exit() {
    let root = short_temp_root("mx-preview-claude-open-stdout");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    seed_claude_direct(&home, Arc::clone(&store)).await;
    let (executable, descendant_pid, parent_pid) = exited_claude_probe_with_inherited_stdout(&root);
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(ControlCodexProbe),
            "/usr/bin/codex".into(),
            Arc::new(ControlNoopUpstream),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), executable),
    );
    let handle = ControlServer::bind_with_activation(&home, store, "routing-test", activation)
        .await
        .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let _ = request(
        &mut stream,
        "open",
        json!({
            "kind": "open-target",
            "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "unset",
                "hostManagedState": "unmanaged",
                "cwd": user_home
            }
        }),
    )
    .await;
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "claude-open-stdout-preview",
            "operation": {
                "kind": "preview-reconciliation",
                "target": "claude",
                "strategy": "reapply"
            }
        }),
    )
    .await
    .unwrap();
    wait_for_path(&descendant_pid).await;
    wait_for_path(&parent_pid).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !process_is_reaped(&parent_pid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Claude probe parent did not exit before disconnect");
    assert_eq!(handle.tracked_inspections(), 1);
    drop(stream);
    tokio::time::timeout(Duration::from_millis(500), async {
        while handle.tracked_inspections() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect did not close the Claude inherited stdout reader");
    handle.shutdown().await.unwrap();
    let descendant_pid = fs::read_to_string(&descendant_pid)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    // SAFETY: the PID was written by the controlled descendant in this test fixture.
    let _ = unsafe { libc::kill(descendant_pid, libc::SIGTERM) };
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn reconciliation_preview_codex_direct_is_read_only_and_emits_no_target_view() {
    let root = short_temp_root("mx-reconcile-preview");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let runtime_probe = Arc::new(RuntimeSourceCodexProbe {
        expected_executable: "/runtime/source/codex".into(),
        calls: AtomicUsize::new(0),
    });
    let activation = Arc::new(ActivationService::new(
        Arc::clone(&store),
        home.clone(),
        runtime_probe.clone(),
        "/runtime/source/codex".into(),
        Arc::new(ControlNoopUpstream),
    ));
    let handle =
        ControlServer::bind_with_activation(&home, Arc::clone(&store), "routing-test", activation)
            .await
            .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(opened["result"]["view"]["managementRevision"], 0);

    let saved = request(
        &mut stream,
        "save",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider", "name": "Codex",
                "baseUrl": "https://api.openai.test/v1", "model": "gpt-test",
                "credential": {"kind": "replace", "value": "preview-secret"},
                "authentication": "openai-bearer", "presetKey": null
            }
        }),
    )
    .await;
    let provider_id = saved["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _saved_push = read_frame(&mut stream).await.unwrap();
    let activated = request(
        &mut stream,
        "activate",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 1,
            "action": {"kind": "activate-provider", "providerId": provider_id, "mode": "direct"}
        }),
    )
    .await;
    let revision = activated["result"]["outcome"]["view"]["managementRevision"]
        .as_u64()
        .unwrap();
    let _activated_push = read_frame(&mut stream).await.unwrap();
    let before_database = secret_file_fingerprint(home.database_path());
    let runtime_config = user_home.join(".codex/config.toml");
    let before_config = secret_file_fingerprint(&runtime_config);

    let preview = request(
        &mut stream,
        "preview",
        json!({
            "kind": "preview-reconciliation", "target": "codex", "strategy": "reapply"
        }),
    )
    .await;

    assert_eq!(preview["type"], "response");
    assert_eq!(preview["result"]["kind"], "reconciliation-preview");
    assert_eq!(preview["result"]["preview"]["target"], "codex");
    assert_eq!(preview["result"]["preview"]["strategy"], "reapply");
    assert_eq!(
        preview["result"]["preview"]["compatibility"],
        json!({
            "version": "shared-runtime-codex 7.4.1",
            "classification": "tested",
            "acknowledgementRequired": false
        })
    );
    assert_eq!(runtime_probe.calls.load(Ordering::SeqCst), 3);
    assert_eq!(preview["result"]["preview"]["managementRevision"], revision);
    assert!(!preview.to_string().contains("preview-secret"));
    assert_secret_file_unchanged(home.database_path(), &before_database);
    assert_secret_file_unchanged(&runtime_config, &before_config);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut stream))
            .await
            .is_err(),
        "preview emitted an unsolicited TargetView"
    );
    assert_eq!(handle.tracked_inspections(), 0);

    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn control_server_rejects_a_server_and_activation_home_mismatch() {
    let root = short_temp_root("mx-runtime-home-mismatch");
    fs::create_dir_all(&root).unwrap();
    let server_home = MuxviaHome::from_root(root.join("server-muxvia")).unwrap();
    let activation_home = MuxviaHome::from_root(root.join("activation-muxvia")).unwrap();
    assert_eq!(server_home.user_home(), activation_home.user_home());
    assert_ne!(server_home.root(), activation_home.root());
    let store = Arc::new(StateStore::open(&server_home).await.unwrap());
    let activation = Arc::new(ActivationService::new(
        Arc::clone(&store),
        activation_home,
        Arc::new(ControlCodexProbe),
        "/runtime/source/codex".into(),
        Arc::new(ControlNoopUpstream),
    ));
    let rejected =
        match ControlServer::bind_with_activation(&server_home, store, "routing-test", activation)
            .await
        {
            Ok(handle) => {
                handle.shutdown().await.unwrap();
                false
            }
            Err(_) => true,
        };
    let _ = fs::remove_dir_all(root);
    assert!(
        rejected,
        "control server accepted a second Home that disagreed with activation"
    );
}

#[tokio::test]
async fn reconciliation_preview_uses_activation_codex_home_override_and_mutates_nothing() {
    let root = short_temp_root("mx-preview-codex-home");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    seed_codex_direct(&home, Arc::clone(&store)).await;
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(ControlCodexProbe),
            "/runtime/source/codex".into(),
            Arc::new(ControlNoopUpstream),
        )
        .with_configuration_home_override(Some(user_home.join("nondefault-codex-home"))),
    );
    let handle = ControlServer::bind_with_activation(
        &home,
        Arc::clone(&store),
        "routing-test",
        Arc::clone(&activation),
    )
    .await
    .unwrap();
    let before_database = secret_file_fingerprint(home.database_path());
    let config = user_home.join(".codex/config.toml");
    let before_config = secret_file_fingerprint(&config);
    let before_view = store.target_view_for(Target::Codex).await.unwrap();
    let mut published = store.subscribe_target_views();
    assert_eq!(activation.model_endpoint_for(Target::Codex).await, None);
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let _opened = request(
        &mut stream,
        "open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let response = request(
        &mut stream,
        "nondefault-home-preview",
        json!({
            "kind": "preview-reconciliation",
            "target": "codex",
            "strategy": "reapply"
        }),
    )
    .await;
    assert_eq!(response["type"], "error");
    assert_eq!(
        response["problem"],
        json!({
            "code": "unsupported-configuration-home",
            "message": "Configuration Home is unsupported"
        })
    );
    assert_eq!(handle.tracked_reconciliation_tokens().await, 0);
    assert_secret_file_unchanged(home.database_path(), &before_database);
    assert_secret_file_unchanged(&config, &before_config);
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        before_view
    );
    assert_eq!(activation.model_endpoint_for(Target::Codex).await, None);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), published.recv())
            .await
            .is_err(),
        "unsupported preview published a TargetView"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut stream))
            .await
            .is_err(),
        "unsupported preview pushed a TargetView"
    );
    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn reconciliation_preview_uses_claude_home_context_and_mutates_nothing() {
    let root = short_temp_root("mx-preview-claude-home");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    seed_claude_direct(&home, Arc::clone(&store)).await;
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(ControlCodexProbe),
            "/usr/bin/codex".into(),
            Arc::new(ControlNoopUpstream),
        )
        .with_claude_runtime(Arc::new(ControlClaudeProbe), "/usr/bin/claude".into()),
    );
    let handle = ControlServer::bind_with_activation(
        &home,
        Arc::clone(&store),
        "routing-test",
        Arc::clone(&activation),
    )
    .await
    .unwrap();
    let before_database = secret_file_fingerprint(home.database_path());
    let config = user_home.join(".claude/settings.json");
    let before_config = secret_file_fingerprint(&config);
    let before_view = store.target_view_for(Target::Claude).await.unwrap();
    let mut published = store.subscribe_target_views();
    assert_eq!(activation.model_endpoint_for(Target::Claude).await, None);
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let _opened = request(
        &mut stream,
        "open",
        json!({
            "kind": "open-target",
            "target": "claude",
            "claudeContext": {
                "claudeConfigDir": user_home.join("nondefault-claude-home"),
                "selectorState": "unset",
                "hostManagedState": "unmanaged",
                "cwd": user_home
            }
        }),
    )
    .await;
    let response = request(
        &mut stream,
        "nondefault-home-preview",
        json!({
            "kind": "preview-reconciliation",
            "target": "claude",
            "strategy": "reapply"
        }),
    )
    .await;
    assert_eq!(response["type"], "error");
    assert_eq!(
        response["problem"],
        json!({
            "code": "unsupported-configuration-home",
            "message": "Configuration Home is unsupported"
        })
    );
    assert_eq!(handle.tracked_reconciliation_tokens().await, 0);
    assert_secret_file_unchanged(home.database_path(), &before_database);
    assert_secret_file_unchanged(&config, &before_config);
    assert_eq!(
        store.target_view_for(Target::Claude).await.unwrap(),
        before_view
    );
    assert_eq!(activation.model_endpoint_for(Target::Claude).await, None);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), published.recv())
            .await
            .is_err(),
        "unsupported preview published a TargetView"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut stream))
            .await
            .is_err(),
        "unsupported preview pushed a TargetView"
    );
    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn claude_gap_reopen_preserves_context_for_takeover_and_publishes_only_claude() {
    let root = short_temp_root("mx-claude-act");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(ControlCodexProbe),
            "/usr/bin/codex".into(),
            Arc::new(ControlNoopUpstream),
        )
        .with_claude_runtime(Arc::new(ControlClaudeProbe), "/usr/bin/claude".into()),
    );
    let handle =
        ControlServer::bind_with_activation(&home, Arc::clone(&store), "routing-test", activation)
            .await
            .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
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
    assert_eq!(opened["result"]["view"]["target"], "claude");
    let saved = request(
        &mut stream,
        "save",
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
    let provider_id = saved["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        read_frame(&mut stream).await.unwrap()["view"]["target"],
        "claude"
    );
    let refreshed = request(
        &mut stream,
        "gap-refresh",
        json!({ "kind": "open-target", "target": "claude" }),
    )
    .await;
    assert_eq!(refreshed["result"]["view"]["target"], "claude");

    let applied = request(
        &mut stream,
        "activate",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": 1,
            "action": {"kind": "activate-provider", "providerId": provider_id, "mode": "takeover"}
        }),
    )
    .await;

    assert_eq!(applied["result"]["outcome"]["status"], "applied");
    let push = read_frame(&mut stream).await.unwrap();
    assert_eq!(push["view"], applied["result"]["outcome"]["view"]);
    assert_eq!(store.target_view().await.unwrap().management_revision, 0);
    assert!(!format!("{opened}{saved}{refreshed}{applied}{push}").contains("provider-secret"));
    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn claude_direct_responds_before_one_push_replays_without_push_and_isolates_codex() {
    let root = short_temp_root("mx-claude-direct");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(ControlCodexProbe),
            "/usr/bin/codex".into(),
            Arc::new(ControlNoopUpstream),
        )
        .with_claude_runtime(Arc::new(ControlClaudeProbe), "/usr/bin/claude".into()),
    );
    let handle =
        ControlServer::bind_with_activation(&home, Arc::clone(&store), "routing-test", activation)
            .await
            .unwrap();
    let mut claude = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut claude).await;
    let opened = request(
        &mut claude,
        "open-claude",
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
    assert_claude_direct_wire_is_secret_free(&opened);
    let mut codex = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut codex).await;
    let codex_opened = request(
        &mut codex,
        "open-codex",
        json!({"kind": "open-target", "target": "codex"}),
    )
    .await;
    assert_claude_direct_wire_is_secret_free(&codex_opened);
    let saved = request(
        &mut claude,
        "save",
        json!({
            "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider", "name": "Claude Direct",
                "baseUrl": "https://api.anthropic.test", "model": "claude-test",
                "credential": {"kind": "replace", "value": "provider-secret-must-not-escape"},
                "authentication": "anthropic-bearer", "presetKey": null
            }
        }),
    )
    .await;
    assert_claude_direct_wire_is_secret_free(&saved);
    let save_push = read_frame(&mut claude).await.unwrap();
    assert_claude_direct_wire_is_secret_free(&save_push);
    let provider_id = saved["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .unwrap();
    let action_id = Uuid::new_v4();

    let applied = request(
        &mut claude,
        "direct",
        json!({
            "kind": "act", "target": "claude", "actionId": action_id,
            "expectedRevision": 1,
            "action": {"kind": "activate-provider", "providerId": provider_id, "mode": "direct"}
        }),
    )
    .await;
    assert_claude_direct_wire_is_secret_free(&applied);
    assert_eq!(applied["result"]["outcome"]["status"], "applied");
    assert_eq!(applied["result"]["outcome"]["view"]["mode"], "direct");
    let push = read_frame(&mut claude).await.unwrap();
    assert_claude_direct_wire_is_secret_free(&push);
    assert_eq!(push["view"], applied["result"]["outcome"]["view"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut codex))
            .await
            .is_err()
    );

    let replay = request(
        &mut claude,
        "replay",
        json!({
            "kind": "act", "target": "claude", "actionId": action_id,
            "expectedRevision": 0,
            "action": {"malformed": true, "credential": "wire-secret-must-not-escape"}
        }),
    )
    .await;
    assert_claude_direct_wire_is_secret_free(&replay);
    assert_eq!(replay["result"]["outcome"]["status"], "replayed");
    assert_eq!(replay["result"]["outcome"]["view"], push["view"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut claude))
            .await
            .is_err()
    );
    assert_eq!(store.target_view().await.unwrap().management_revision, 0);
    handle.shutdown().await.unwrap();
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn target_isolation_opens_claude_without_touching_codex_state() {
    let mut fixture = ControlFixture::start().await;
    let before = fixture.store.target_view().await.unwrap();
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;

    let unopened = request(
        &mut stream,
        "before-open",
        json!({ "kind": "discover-models", "target": "claude", "source": {
            "kind": "draft", "baseUrl": "https://api.anthropic.com/v1",
            "authentication": "anthropic-api-key",
            "credentialSource": { "kind": "missing" }
        }}),
    )
    .await;
    assert_eq!(unopened["problem"]["code"], "target-not-open");

    let response = request(
        &mut stream,
        "claude-open",
        json!({ "kind": "open-target", "target": "claude" }),
    )
    .await;
    assert_eq!(response["type"], "response");
    assert_eq!(response["result"]["kind"], "target-view");
    assert_eq!(response["result"]["view"]["target"], "claude");
    assert_eq!(fixture.store.target_view().await.unwrap(), before);
    fixture.shutdown().await;
}

#[tokio::test]
async fn target_isolation_scopes_pushes_actions_and_receipts_to_the_opened_target() {
    let mut fixture = ControlFixture::start().await;
    let mut codex = fixture.connect().await;
    let mut claude = fixture.connect().await;
    hello(&mut codex).await;
    hello(&mut claude).await;
    request(
        &mut codex,
        "open-codex",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    request(
        &mut claude,
        "open-claude",
        json!({ "kind": "open-target", "target": "claude" }),
    )
    .await;

    let action_id = Uuid::new_v4();
    write_frame(&mut claude, &json!({
        "type": "request", "requestId": "claude-create",
        "operation": { "kind": "act", "target": "claude", "actionId": action_id, "expectedRevision": 0,
            "action": { "kind": "create-provider", "name": "Claude", "baseUrl": "https://api.anthropic.com/v1",
                "model": "claude-test", "credential": { "kind": "replace", "value": "claude-secret" },
                "authentication": "anthropic-api-key", "presetKey": "anthropic-api-messages" } }
    })).await.unwrap();
    let response = read_frame(&mut claude).await.unwrap();
    let push = read_frame(&mut claude).await.unwrap();
    assert_eq!(response["result"]["outcome"]["view"]["target"], "claude");
    assert_eq!(push["view"]["target"], "claude");
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut codex))
            .await
            .is_err()
    );

    let codex_response = request(
        &mut codex,
        "codex-create",
        json!({
            "kind": "act", "target": "codex", "actionId": action_id, "expectedRevision": 0,
            "action": create_action("Codex", "codex-secret")
        }),
    )
    .await;
    assert_eq!(codex_response["result"]["outcome"]["status"], "applied");
    let codex_push = read_frame(&mut codex).await.unwrap();
    assert_eq!(codex_push["view"]["target"], "codex");
    assert_eq!(
        fixture
            .store
            .target_view_for(muxvia_routing::control::protocol::Target::Claude)
            .await
            .unwrap()
            .management_revision,
        1
    );
    assert_eq!(
        fixture
            .store
            .target_view()
            .await
            .unwrap()
            .management_revision,
        1
    );
    let mismatch = request(
        &mut claude,
        "cross-target",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(
        mismatch["problem"]["code"],
        "target-session-target-mismatch"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn claude_receipts_replay_before_malformed_actions_or_duplicate_pushes() {
    let mut fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open-claude",
        json!({ "kind": "open-target", "target": "claude" }),
    )
    .await;

    let malformed = request(
        &mut stream,
        "claude-malformed-before-context",
        json!({
            "kind":"act","target":"claude","actionId":Uuid::new_v4(),
            "expectedRevision":0,
            "action":{"kind":"create-provider","name":42}
        }),
    )
    .await;
    assert_eq!(malformed["problem"]["code"], "invalid-provider");

    let action_id = Uuid::new_v4();
    write_frame(
        &mut stream,
        &json!({
            "type": "request", "requestId": "claude-first",
            "operation": {
                "kind": "act", "target": "claude", "actionId": action_id, "expectedRevision": 0,
                "action": {
                    "kind": "create-provider", "name": "Claude", "baseUrl": "https://api.anthropic.com/v1",
                    "model": "claude-test", "credential": { "kind": "replace", "value": "claude-secret" },
                    "authentication": "anthropic-api-key", "presetKey": "anthropic-api-messages"
                }
            }
        }),
    )
    .await
    .unwrap();
    let applied = read_frame(&mut stream).await.unwrap();
    let push = read_frame(&mut stream).await.unwrap();
    let before = fixture
        .store
        .target_view_for(muxvia_routing::control::protocol::Target::Claude)
        .await
        .unwrap();

    write_frame(
        &mut stream,
        &json!({
            "type": "request", "requestId": "claude-replay",
            "operation": {
                "kind": "act", "target": "claude", "actionId": action_id, "expectedRevision": 999,
                "action": { "kind": "activate-provider", "providerId": "not-a-uuid", "mode": "takeover" }
            }
        }),
    )
    .await
    .unwrap();
    let replayed = read_frame(&mut stream).await.unwrap();
    assert_eq!(replayed["result"]["kind"], "action-outcome");
    assert_eq!(replayed["result"]["outcome"]["status"], "replayed");
    assert_eq!(
        replayed["result"]["outcome"]["view"],
        applied["result"]["outcome"]["view"]
    );
    assert_eq!(push["view"], applied["result"]["outcome"]["view"]);
    assert_eq!(
        fixture
            .store
            .target_view_for(muxvia_routing::control::protocol::Target::Claude)
            .await
            .unwrap(),
        before
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut stream))
            .await
            .is_err()
    );
    fixture.shutdown().await;
}

async fn wait_for_inspections(fixture: &ControlFixture, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if fixture.handle.as_ref().unwrap().tracked_inspections() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn wait_for_zero_inspections(fixture: &ControlFixture) {
    wait_for_inspections(fixture, 0).await;
}

async fn backpressure_reconciliation_fixture() -> ControlFixture {
    let root = short_temp_root("mx-writer-bound");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    seed_codex_direct(&home, Arc::clone(&store)).await;
    let revision = store.target_view().await.unwrap().management_revision;
    store
        .apply_provider_action(
            Uuid::new_v4(),
            revision,
            json!({
                "kind": "create-provider",
                "name": "Large response",
                "baseUrl": "https://provider.example/v1",
                "model": "m".repeat(128 * 1024),
                "credential": { "kind": "replace", "value": "large-view-secret" },
                "presetKey": null,
            }),
        )
        .await
        .unwrap();
    let activation = Arc::new(ActivationService::new(
        Arc::clone(&store),
        home.clone(),
        Arc::new(ControlCodexProbe),
        "/usr/bin/codex".into(),
        Arc::new(ControlNoopUpstream),
    ));
    let handle =
        ControlServer::bind_with_activation(&home, Arc::clone(&store), "routing-test", activation)
            .await
            .unwrap();
    ControlFixture {
        root,
        store,
        handle: Some(handle),
    }
}

async fn opened_reconciliation_session(fixture: &ControlFixture) -> UnixStream {
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open-reconciliation",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(opened["type"], "response");
    stream
}

async fn preview_token(stream: &mut UnixStream, request_id: &str, strategy: &str) -> Uuid {
    request(
        stream,
        request_id,
        json!({
            "kind": "preview-reconciliation",
            "target": "codex",
            "strategy": strategy
        }),
    )
    .await["result"]["preview"]["observationToken"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

async fn backpressured_preview_session(fixture: &ControlFixture, request_id: &str) -> UnixStream {
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    for index in 0..8 {
        write_frame(
            &mut stream,
            &json!({
                "type": "request",
                "requestId": format!("{request_id}-fill-{index}"),
                "operation": { "kind": "open-target", "target": "codex" }
            }),
        )
        .await
        .unwrap();
    }
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": request_id,
            "operation": {
                "kind": "preview-reconciliation",
                "target": "codex",
                "strategy": "reapply"
            }
        }),
    )
    .await
    .unwrap();
    stream
}

async fn wait_until_token_is_not(
    fixture: &ControlFixture,
    strategy: ReconciliationStrategy,
    prior: Uuid,
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture
            .handle
            .as_ref()
            .unwrap()
            .tracks_reconciliation_token(Target::Codex, strategy, prior)
            .await
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("backpressured preview did not register");
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
async fn shutdown_is_bounded_when_an_authorized_peer_stops_reading() {
    let mut upstream = HeldInspectionServer::start().await;
    let mut fixture = ControlFixture::start().await;
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    seed_codex_direct(&home, Arc::clone(&fixture.store)).await;
    let revision = fixture
        .store
        .target_view()
        .await
        .unwrap()
        .management_revision;
    fixture
        .store
        .apply_provider_action(
            Uuid::new_v4(),
            revision,
            json!({
                "kind": "create-provider",
                "name": "Large response",
                "baseUrl": "https://provider.example/v1",
                "model": "m".repeat(128 * 1024),
                "credential": { "kind": "replace", "value": "large-view-secret" },
                "presetKey": null,
            }),
        )
        .await
        .unwrap();

    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    write_frame(
        &mut stream,
        &json!({
            "type": "request", "requestId": "open-before-backpressure",
            "operation": { "kind": "open-target", "target": "codex" }
        }),
    )
    .await
    .unwrap();
    for index in 0..8 {
        write_frame(
            &mut stream,
            &json!({
                "type": "request",
                "requestId": format!("fill-writer-{index}"),
                "operation": { "kind": "open-target", "target": "codex" }
            }),
        )
        .await
        .unwrap();
    }
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "preview-behind-backpressure",
            "operation": {
                "kind": "preview-reconciliation",
                "target": "codex",
                "strategy": "reapply"
            }
        }),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture
            .handle
            .as_ref()
            .unwrap()
            .tracked_reconciliation_tokens()
            .await
            != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("preview did not register behind the blocked writer");
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "held-during-backpressure",
            "operation": {
                "kind": "discover-models",
                "target": "codex",
                "source": {
                    "kind": "draft",
                    "baseUrl": upstream.base_url,
                    "authentication": "openai-bearer",
                    "credentialSource": {
                        "kind": "ephemeral",
                        "value": "backpressure-secret-must-not-escape"
                    }
                }
            }
        }),
    )
    .await
    .unwrap();
    upstream.wait_started().await;
    for index in 0..96 {
        write_frame(
            &mut stream,
            &json!({
                "type": "request",
                "requestId": format!("unread-{index}"),
                "operation": { "kind": "open-target", "target": "codex" }
            }),
        )
        .await
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    fixture.handle.as_mut().unwrap().request_shutdown();
    upstream.wait_dropped().await;
    wait_for_zero_inspections(&fixture).await;
    assert_eq!(
        fixture
            .handle
            .as_ref()
            .unwrap()
            .tracked_reconciliation_tokens()
            .await,
        0,
        "shutdown retained a preview token that its blocked writer never delivered"
    );
    tokio::time::timeout(Duration::from_secs(1), fixture.shutdown())
        .await
        .expect("Control Server shutdown waited on a non-reading peer")
}

#[tokio::test]
async fn backpressured_preview_times_out_and_restores_the_prior_disclosed_token() {
    let mut fixture = backpressure_reconciliation_fixture().await;
    let mut disclosed_session = opened_reconciliation_session(&fixture).await;
    let disclosed = preview_token(&mut disclosed_session, "disclosed-preview", "reapply").await;
    assert!(
        fixture
            .handle
            .as_ref()
            .unwrap()
            .tracks_reconciliation_token(Target::Codex, ReconciliationStrategy::Reapply, disclosed,)
            .await
    );

    let mut blocked = backpressured_preview_session(&fixture, "blocked-preview").await;
    wait_until_token_is_not(&fixture, ReconciliationStrategy::Reapply, disclosed).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.handle.as_ref().unwrap().tracked_writers() != 1 {
            assert!(
                !fixture
                    .handle
                    .as_ref()
                    .unwrap()
                    .tracks_reconciliation_token(
                        Target::Codex,
                        ReconciliationStrategy::Reapply,
                        disclosed,
                    )
                    .await,
                "the prior token became visible before the blocked writer terminated"
            );
            tokio::task::yield_now().await;
        }
        while fixture.handle.as_ref().unwrap().tracked_sessions() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the blocked writer/session did not terminate before rollback was observed");
    let mut queued_bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(1),
        blocked.read_to_end(&mut queued_bytes),
    )
    .await
    .expect("the terminated preview session did not reach EOF")
    .expect("failed to drain the terminated preview session");
    let mut queued = queued_bytes.as_slice();
    while queued.len() >= 4 {
        let length = u32::from_be_bytes(queued[..4].try_into().unwrap()) as usize;
        queued = &queued[4..];
        if queued.len() < length {
            break;
        }
        let frame: Value = serde_json::from_slice(&queued[..length])
            .expect("the blocked writer emitted a malformed complete frame");
        assert_ne!(
            frame.get("requestId").and_then(Value::as_str),
            Some("blocked-preview"),
            "the timed-out preview response was delivered after its registration rolled back"
        );
        queued = &queued[length..];
    }
    assert_eq!(fixture.handle.as_ref().unwrap().tracked_inspections(), 0);
    assert!(
        fixture
            .handle
            .as_ref()
            .unwrap()
            .tracks_reconciliation_token(Target::Codex, ReconciliationStrategy::Reapply, disclosed)
            .await,
        "the writer/session closed without restoring the prior disclosed token"
    );
    assert_eq!(
        fixture
            .handle
            .as_ref()
            .unwrap()
            .tracked_reconciliation_tokens()
            .await,
        1
    );
    assert!(
        fixture
            .handle
            .as_ref()
            .unwrap()
            .validates_reconciliation_token(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                disclosed,
                None,
            )
            .await,
        "the exact prior disclosed token did not validate after rollback"
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn backpressured_same_key_serializes_without_blocking_a_different_key() {
    let mut fixture = backpressure_reconciliation_fixture().await;
    let mut responsive = opened_reconciliation_session(&fixture).await;
    let disclosed = preview_token(&mut responsive, "disclosed-preview", "reapply").await;
    let _blocked = backpressured_preview_session(&fixture, "blocked-preview").await;
    wait_until_token_is_not(&fixture, ReconciliationStrategy::Reapply, disclosed).await;

    write_frame(
        &mut responsive,
        &json!({
            "type": "request",
            "requestId": "same-key-preview",
            "operation": {
                "kind": "preview-reconciliation",
                "target": "codex",
                "strategy": "reapply"
            }
        }),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while fixture.handle.as_ref().unwrap().tracked_inspections() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("same-key preview did not serialize behind the pending registration");
    write_frame(
        &mut responsive,
        &json!({
            "type": "request",
            "requestId": "different-key-preview",
            "operation": {
                "kind": "preview-reconciliation",
                "target": "codex",
                "strategy": "adopt"
            }
        }),
    )
    .await
    .unwrap();

    let different_key =
        tokio::time::timeout(Duration::from_millis(100), read_frame(&mut responsive))
            .await
            .expect("a blocked reapply registration starved an adopt preview")
            .unwrap();
    assert_eq!(different_key["requestId"], "different-key-preview");
    let different_key_token: Uuid = different_key["result"]["preview"]["observationToken"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let same_key = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut responsive))
        .await
        .expect("same-key preview did not resume after the writer bound")
        .unwrap();
    assert_eq!(same_key["requestId"], "same-key-preview");
    let same_key_token: Uuid = same_key["result"]["preview"]["observationToken"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let handle = fixture.handle.as_ref().unwrap();
    assert!(
        handle
            .validates_reconciliation_token(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                same_key_token,
                None,
            )
            .await
    );
    assert!(
        handle
            .validates_reconciliation_token(
                Target::Codex,
                ReconciliationStrategy::Adopt,
                different_key_token,
                None,
            )
            .await
    );
    assert_eq!(handle.tracked_reconciliation_tokens().await, 2);

    fixture.shutdown().await;
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
async fn incomplete_claude_selector_context_is_rejected_at_the_real_uds_boundary() {
    let fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "missing-selector",
            "operation": {
                "kind": "open-target",
                "target": "claude",
                "claudeContext": {
                    "claudeConfigDir": null,
                    "selectorState": "enabled",
                    "hostManagedState": "unmanaged",
                    "cwd": "/safe/project"
                }
            }
        }),
    )
    .await
    .unwrap();

    let reply = read_frame(&mut stream).await.unwrap();
    assert_eq!(reply["type"], "error");
    assert_eq!(reply["requestId"], "missing-selector");
    assert_eq!(reply["problem"]["code"], "unsupported-operation");
    assert!(!reply.to_string().contains("undefined"));

    let opened = request(
        &mut stream,
        "valid-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(opened["type"], "response");
}

#[tokio::test]
async fn draft_discovery_rejects_cross_target_authentication_before_upstream() {
    let mut upstream = CountingInspectionServer::start().await;
    let fixture = ControlFixture::start().await;

    for (target, authentication) in [
        ("claude", "openai-bearer"),
        ("codex", "anthropic-api-key"),
        ("codex", "anthropic-bearer"),
    ] {
        let mut stream = fixture.connect().await;
        hello(&mut stream).await;
        let claude_context = (target == "claude").then(|| {
            json!({
                "claudeConfigDir": null,
                "selectorState": "unset",
                "blockingSelector": null,
                "hostManagedState": "unmanaged",
                "cwd": "/safe/project"
            })
        });
        let opened = request(
            &mut stream,
            &format!("open-{target}"),
            json!({
                "kind": "open-target",
                "target": target,
                "claudeContext": claude_context
            }),
        )
        .await;
        assert_eq!(opened["type"], "response");

        let secret = format!("{target}-{authentication}-secret-must-not-escape");
        let rejected = request(
            &mut stream,
            &format!("invalid-{target}-{authentication}"),
            json!({
                "kind": "discover-models",
                "target": target,
                "source": {
                    "kind": "draft",
                    "baseUrl": upstream.base_url,
                    "authentication": authentication,
                    "credentialSource": { "kind": "ephemeral", "value": secret }
                }
            }),
        )
        .await;
        assert_eq!(rejected["type"], "error");
        assert_eq!(
            rejected["problem"]["code"],
            "invalid-provider-authentication"
        );
        assert_eq!(
            rejected["problem"]["message"],
            "Draft authentication does not match Target"
        );
        assert!(!rejected.to_string().contains(&secret));
    }

    upstream.assert_no_completion().await;
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
async fn failover_draft_save_responds_before_one_push_and_changes_no_live_route() {
    let mut fixture = ControlFixture::start().await;
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    seed_codex_direct(&home, Arc::clone(&fixture.store)).await;
    let activated = fixture.store.target_view_for(Target::Codex).await.unwrap();
    let fallback = fixture
        .store
        .apply_provider_action_for(
            Target::Codex,
            Uuid::new_v4(),
            activated.management_revision,
            json!({
                "kind": "create-provider",
                "name": "Fallback",
                "baseUrl": "https://fallback.test/v1",
                "model": "fallback-model",
                "credential": { "kind": "replace", "value": "FAILOVER_DRAFT_SECRET" },
                "authentication": "openai-bearer",
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    let before = fallback.view;
    let current = before.current_provider_id.clone().unwrap();
    let current_revision = before
        .providers
        .iter()
        .find(|provider| provider.id.to_string() == current)
        .unwrap()
        .provider_revision;
    let fallback_provider = before
        .providers
        .iter()
        .find(|provider| provider.name == "Fallback")
        .unwrap();

    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "failover-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let action_id = Uuid::new_v4();
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "failover-save",
            "operation": {
                "kind": "act",
                "target": "codex",
                "actionId": action_id,
                "expectedRevision": before.management_revision,
                "action": {
                    "kind": "save-failover-draft",
                    "members": [
                        { "providerId": current, "providerRevision": current_revision },
                        { "providerId": fallback_provider.id, "providerRevision": fallback_provider.provider_revision }
                    ]
                }
            }
        }),
    )
    .await
    .unwrap();
    let response = read_frame(&mut stream).await.unwrap();
    assert!(
        response["type"] == "response",
        "draft save did not return a response"
    );
    let push = read_frame(&mut stream).await.unwrap();
    let view = &response["result"]["outcome"]["view"];
    assert!(
        push["view"] == *view,
        "draft save push did not match its response"
    );
    assert_eq!(
        view["failover"]["draftRevision"].as_u64(),
        Some(before.failover.draft_revision + 1),
        "saved draft revision was not projected"
    );
    assert_eq!(
        view["failover"]["draftMembers"].as_array().map(Vec::len),
        Some(2),
        "saved draft members were not projected"
    );
    assert!(
        view["currentProviderId"] == serde_json::to_value(&before.current_provider_id).unwrap()
            && view["servingProviderId"]
                == serde_json::to_value(&before.serving_provider_id).unwrap()
            && view["activatedSnapshot"]
                == serde_json::to_value(&before.activated_snapshot).unwrap()
            && view["mode"] == before.mode,
        "draft save changed the live route"
    );
    assert!(
        !format!("{response:?}{push:?}").contains("FAILOVER_DRAFT_SECRET"),
        "draft response exposed a Provider credential"
    );

    let replay = request(
        &mut stream,
        "failover-replay",
        json!({
            "kind": "act", "target": "codex", "actionId": action_id,
            "expectedRevision": 0,
            "action": { "kind": "save-failover-draft", "members": [] }
        }),
    )
    .await;
    assert!(
        replay["result"]["outcome"]["status"] == "replayed",
        "draft replay was not receipt-first"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut stream))
            .await
            .is_err(),
        "draft replay published a duplicate view"
    );

    let apply_action_id = Uuid::new_v4();
    let config_path = fixture.root.join("home/.codex/config.toml");
    let exact_config = fs::read(&config_path).unwrap();
    let config_text = String::from_utf8(exact_config.clone()).unwrap();
    let model_line = config_text
        .lines()
        .find(|line| line.starts_with("model = "))
        .expect("managed configuration must contain the selected model");
    let drifted_config = config_text.replacen(model_line, "model = \"stale-external-drift\"", 1);
    fs::write(&config_path, drifted_config).unwrap();
    let stale_apply = request(
        &mut stream,
        "failover-stale-apply",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 0,
            "action": {
                "kind": "apply-failover-chain",
                "draftRevision": before.failover.draft_revision + 1
            }
        }),
    )
    .await;
    assert!(
        stale_apply["type"] == "error" && stale_apply["problem"]["code"] == "stale-revision",
        "stale Apply did not reject before live managed-state observation"
    );
    let after_stale = fixture.store.target_view_for(Target::Codex).await.unwrap();
    assert!(
        after_stale.management_revision == view["managementRevision"].as_u64().unwrap()
            && after_stale.managed_configuration.state == "applied"
            && after_stale.problems.is_empty(),
        "stale Apply persisted a managed-write blocker"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut stream))
            .await
            .is_err(),
        "stale Apply published a Target View"
    );
    fs::write(&config_path, exact_config).unwrap();
    write_frame(
        &mut stream,
        &json!({
            "type": "request", "requestId": "failover-apply",
            "operation": {
                "kind": "act", "target": "codex", "actionId": apply_action_id,
                "expectedRevision": view["managementRevision"],
                "action": {
                    "kind": "apply-failover-chain",
                    "draftRevision": before.failover.draft_revision + 1
                }
            }
        }),
    )
    .await
    .unwrap();
    let applied = read_frame(&mut stream).await.unwrap();
    assert!(
        applied["type"] == "response",
        "Failover Apply did not return a response"
    );
    let applied_push = read_frame(&mut stream).await.unwrap();
    let applied_view = &applied["result"]["outcome"]["view"];
    assert!(
        applied_push["view"] == *applied_view
            && applied_view["failover"]["activePlan"]["members"]
                .as_array()
                .is_some_and(|members| members.len() == 2),
        "Failover Apply did not publish its immutable plan"
    );
    assert!(
        applied_view["currentProviderId"] == view["currentProviderId"]
            && applied_view["servingProviderId"] == view["servingProviderId"]
            && applied_view["activatedSnapshot"] == view["activatedSnapshot"],
        "Failover Apply changed the live route"
    );
    let apply_replay = request(
        &mut stream,
        "failover-apply-replay",
        json!({
            "kind": "act", "target": "codex", "actionId": apply_action_id,
            "expectedRevision": 0,
            "action": { "kind": "apply-failover-chain", "draftRevision": 999 }
        }),
    )
    .await;
    assert!(
        apply_replay["result"]["outcome"]["status"] == "replayed",
        "Failover Apply did not replay receipt-first"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut stream))
            .await
            .is_err(),
        "Failover Apply replay published a duplicate view"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn reconciliation_publication_waits_for_the_initiating_action_response_writer_ack() {
    let mut fixture = ControlFixture::start().await;
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    seed_codex_direct(&home, Arc::clone(&fixture.store)).await;
    inflate_codex_target_view(&fixture.store).await;
    let config_path = fixture.root.join("home/.codex/config.toml");
    fs::write(
        &config_path,
        fs::read_to_string(&config_path)
            .unwrap()
            .replace("seed-model", "writer-ack-reapply-drift"),
    )
    .unwrap();

    let mut initiator = fixture.connect().await;
    hello(&mut initiator).await;
    let opened = request(
        &mut initiator,
        "initiator-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let preview = request(
        &mut initiator,
        "writer-ack-preview",
        json!({ "kind": "preview-reconciliation", "target": "codex", "strategy": "reapply" }),
    )
    .await;
    let mut subscriber = fixture.connect().await;
    hello(&mut subscriber).await;
    request(
        &mut subscriber,
        "subscriber-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    queue_writer_backpressure(&mut initiator, "reconcile-fill").await;
    let action_id = Uuid::new_v4();
    write_frame(
        &mut initiator,
        &json!({
            "type": "request", "requestId": "writer-ack-reconcile",
            "operation": {
                "kind": "act", "target": "codex", "actionId": action_id,
                "expectedRevision": opened["result"]["view"]["managementRevision"],
                "action": {
                    "kind": "reconcile", "strategy": "reapply",
                    "observationToken": preview["result"]["preview"]["observationToken"]
                }
            }
        }),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_millis(150), async {
        while fixture
            .store
            .receipt_for(Target::Codex, action_id)
            .await
            .unwrap()
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconciliation did not durably commit behind the blocked writer");
    assert!(
        tokio::time::timeout(Duration::from_millis(40), read_frame(&mut subscriber))
            .await
            .is_err(),
        "subscriber received reconciliation publication before action response writer ack"
    );

    let newest = fixture.store.target_view_for(Target::Codex).await.unwrap();
    let newer = request(
        &mut subscriber,
        "newer-provider",
        json!({
            "kind":"act","target":"codex","actionId":Uuid::new_v4(),
            "expectedRevision":newest.management_revision,
            "action":create_action("Newer provider", "NEWER_SECRET_97301")
        }),
    )
    .await;
    assert_eq!(newer["type"], "response");
    let newer_push = read_frame(&mut subscriber).await.unwrap();
    assert_eq!(newer_push["view"], newer["result"]["outcome"]["view"]);
    assert!(
        newer_push["view"]["viewSequence"].as_u64().unwrap()
            > fixture
                .store
                .receipt_for(Target::Codex, action_id)
                .await
                .unwrap()
                .unwrap()
                .view
                .view_sequence
    );

    let mut response = None;
    for _ in 0..9 {
        let frame = read_frame(&mut initiator).await.unwrap();
        if frame["requestId"] == "writer-ack-reconcile" {
            response = Some(frame);
        }
    }
    let response = response.expect("initiating reconciliation response was not written");
    assert_eq!(response["type"], "response");
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut subscriber))
            .await
            .is_err(),
        "late old writer ack published a regressing reconciliation view"
    );
    let replay = request(
        &mut subscriber,
        "superseded-replay",
        json!({
            "kind":"act","target":"codex","actionId":action_id,
            "expectedRevision":0,
            "action":create_action("ignored replay", "IGNORED_REPLAY_SECRET_97304")
        }),
    )
    .await;
    assert_eq!(replay["result"]["outcome"]["status"], "replayed");
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut subscriber))
            .await
            .is_err(),
        "receipt replay published after its original view was superseded"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn late_ack_does_not_publish_before_a_newer_durable_writer_failure() {
    let mut fixture = ControlFixture::start().await;
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    seed_codex_direct(&home, Arc::clone(&fixture.store)).await;
    inflate_codex_target_view(&fixture.store).await;
    let config_path = fixture.root.join("home/.codex/config.toml");
    fs::write(
        &config_path,
        fs::read_to_string(&config_path)
            .unwrap()
            .replace("seed-model", "older-writer-ack-drift"),
    )
    .unwrap();

    let mut older = fixture.connect().await;
    hello(&mut older).await;
    let opened = request(
        &mut older,
        "older-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let preview = request(
        &mut older,
        "older-preview",
        json!({ "kind": "preview-reconciliation", "target": "codex", "strategy": "reapply" }),
    )
    .await;
    let mut newer = fixture.connect().await;
    hello(&mut newer).await;
    request(
        &mut newer,
        "newer-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    queue_writer_backpressure(&mut older, "older-fill").await;
    let older_action_id = Uuid::new_v4();
    write_frame(
        &mut older,
        &json!({
            "type": "request", "requestId": "older-reapply",
            "operation": {
                "kind": "act", "target": "codex", "actionId": older_action_id,
                "expectedRevision": opened["result"]["view"]["managementRevision"],
                "action": {
                    "kind": "reconcile", "strategy": "reapply",
                    "observationToken": preview["result"]["preview"]["observationToken"]
                }
            }
        }),
    )
    .await
    .unwrap();
    let older_durable = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            if let Some(outcome) = fixture
                .store
                .receipt_for(Target::Codex, older_action_id)
                .await
                .unwrap()
            {
                break outcome;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("older reconciliation did not durably commit behind the blocked writer");

    fs::write(
        &config_path,
        fs::read_to_string(&config_path)
            .unwrap()
            .replace("seed-model", "newer-writer-failure-drift"),
    )
    .unwrap();
    let newer_preview = request(
        &mut newer,
        "newer-preview",
        json!({ "kind": "preview-reconciliation", "target": "codex", "strategy": "reapply" }),
    )
    .await;
    queue_writer_backpressure(&mut newer, "newer-fill").await;
    let newer_action_id = Uuid::new_v4();
    write_frame(
        &mut newer,
        &json!({
            "type": "request", "requestId": "newer-reapply",
            "operation": {
                "kind": "act", "target": "codex", "actionId": newer_action_id,
                "expectedRevision": older_durable.view.management_revision,
                "action": {
                    "kind": "reconcile", "strategy": "reapply",
                    "observationToken": newer_preview["result"]["preview"]["observationToken"]
                }
            }
        }),
    )
    .await
    .unwrap();
    let newer_durable = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            if let Some(outcome) = fixture
                .store
                .receipt_for(Target::Codex, newer_action_id)
                .await
                .unwrap()
            {
                break outcome;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("newer reconciliation did not durably commit behind the failing writer");
    assert!(newer_durable.view.view_sequence > older_durable.view.view_sequence);
    drop(newer);

    let mut older_response = None;
    for _ in 0..9 {
        let frame = read_frame(&mut older).await.unwrap();
        if frame["requestId"] == "older-reapply" {
            older_response = Some(frame);
        }
    }
    assert_eq!(
        older_response.expect("older response was not written")["type"],
        "response"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut older))
            .await
            .is_err(),
        "late acknowledgement published a view older than durable state"
    );

    let mut reopened = fixture.connect().await;
    hello(&mut reopened).await;
    let visible = request(
        &mut reopened,
        "open-after-newer-writer-failure",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(
        visible["result"]["view"],
        serde_json::to_value(newer_durable.view).unwrap()
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn durable_live_drift_publication_waits_for_the_initiating_error_writer_ack() {
    let mut fixture = ControlFixture::start().await;
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    seed_codex_direct(&home, Arc::clone(&fixture.store)).await;
    inflate_codex_target_view(&fixture.store).await;
    let config_path = fixture.root.join("home/.codex/config.toml");
    fs::write(
        &config_path,
        fs::read_to_string(&config_path)
            .unwrap()
            .replace("seed-model", "writer-ack-live-drift"),
    )
    .unwrap();

    let mut initiator = fixture.connect().await;
    hello(&mut initiator).await;
    let opened = request(
        &mut initiator,
        "drift-initiator-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let mut subscriber = fixture.connect().await;
    hello(&mut subscriber).await;
    request(
        &mut subscriber,
        "drift-subscriber-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    queue_writer_backpressure(&mut initiator, "drift-fill").await;
    write_frame(
        &mut initiator,
        &json!({
            "type": "request", "requestId": "writer-ack-drift",
            "operation": {
                "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
                "expectedRevision": opened["result"]["view"]["managementRevision"],
                "action": {
                    "kind": "create-provider", "name": "must-not-create",
                    "baseUrl": "https://must-not-create.test/v1", "model": "none",
                    "credential": { "kind": "replace", "value": "must-not-create-secret" },
                    "authentication": "openai-bearer", "presetKey": null
                }
            }
        }),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            let view = fixture.store.target_view_for(Target::Codex).await.unwrap();
            if view
                .problems
                .iter()
                .any(|problem| problem.code == "configuration-drift")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("live drift was not durably recorded behind the blocked writer");
    assert!(
        tokio::time::timeout(Duration::from_millis(40), read_frame(&mut subscriber))
            .await
            .is_err(),
        "subscriber received live-drift publication before error writer ack"
    );

    let preview = request(
        &mut subscriber,
        "newer-reapply-preview",
        json!({"kind":"preview-reconciliation","target":"codex","strategy":"reapply"}),
    )
    .await;
    let newest = fixture.store.target_view_for(Target::Codex).await.unwrap();
    let newer = request(
        &mut subscriber,
        "newer-reapply",
        json!({
            "kind":"act","target":"codex","actionId":Uuid::new_v4(),
            "expectedRevision":newest.management_revision,
            "action":{"kind":"reconcile","strategy":"reapply",
                "observationToken":preview["result"]["preview"]["observationToken"]}
        }),
    )
    .await;
    assert_eq!(newer["type"], "response");
    let newer_push = read_frame(&mut subscriber).await.unwrap();
    assert_eq!(newer_push["view"], newer["result"]["outcome"]["view"]);

    let mut error = None;
    for _ in 0..9 {
        let frame = read_frame(&mut initiator).await.unwrap();
        if frame["requestId"] == "writer-ack-drift" {
            error = Some(frame);
        }
    }
    let error = error.expect("initiating live-drift error was not written");
    assert_eq!(error["problem"]["code"], "configuration-drift");
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut subscriber))
            .await
            .is_err(),
        "late old writer ack published a regressing live-drift view"
    );
    let initiating_push = read_frame(&mut initiator).await.unwrap();
    assert_eq!(initiating_push["type"], "target-view");
    let repeated = request(
        &mut initiator,
        "already-durable-drift",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": opened["result"]["view"]["managementRevision"],
            "action": {
                "kind": "create-provider", "name": "still-must-not-create",
                "baseUrl": "https://still-must-not-create.test/v1", "model": "none",
                "credential": { "kind": "replace", "value": "still-must-not-create-secret" },
                "authentication": "openai-bearer", "presetKey": null
            }
        }),
    )
    .await;
    assert_eq!(repeated["problem"]["code"], "stale-revision");
    assert!(
        tokio::time::timeout(Duration::from_millis(80), read_frame(&mut subscriber))
            .await
            .is_err(),
        "already-durable drift was published a second time"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn reconciliation_writer_failure_suppresses_publication_but_next_open_reads_durable_state() {
    let mut fixture = ControlFixture::start().await;
    let home = MuxviaHome::from_user_home(&fixture.root.join("home"));
    seed_codex_direct(&home, Arc::clone(&fixture.store)).await;
    inflate_codex_target_view(&fixture.store).await;
    let config_path = fixture.root.join("home/.codex/config.toml");
    fs::write(
        &config_path,
        fs::read_to_string(&config_path)
            .unwrap()
            .replace("seed-model", "writer-failure-reapply-drift"),
    )
    .unwrap();

    let mut initiator = fixture.connect().await;
    hello(&mut initiator).await;
    let opened = request(
        &mut initiator,
        "failure-initiator-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let preview = request(
        &mut initiator,
        "failure-preview",
        json!({ "kind": "preview-reconciliation", "target": "codex", "strategy": "reapply" }),
    )
    .await;
    let mut subscriber = fixture.connect().await;
    hello(&mut subscriber).await;
    request(
        &mut subscriber,
        "failure-subscriber-open",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    queue_writer_backpressure(&mut initiator, "failure-fill").await;
    let action_id = Uuid::new_v4();
    write_frame(
        &mut initiator,
        &json!({
            "type": "request", "requestId": "writer-failure-reconcile",
            "operation": {
                "kind": "act", "target": "codex", "actionId": action_id,
                "expectedRevision": opened["result"]["view"]["managementRevision"],
                "action": {
                    "kind": "reconcile", "strategy": "reapply",
                    "observationToken": preview["result"]["preview"]["observationToken"]
                }
            }
        }),
    )
    .await
    .unwrap();
    let durable = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            if let Some(outcome) = fixture
                .store
                .receipt_for(Target::Codex, action_id)
                .await
                .unwrap()
            {
                break outcome;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconciliation did not durably commit behind the failing writer");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(40), read_frame(&mut subscriber))
            .await
            .is_err(),
        "writer failure emitted a misleading reconciliation publication"
    );

    let mut reopened = fixture.connect().await;
    hello(&mut reopened).await;
    let visible = request(
        &mut reopened,
        "open-after-writer-failure",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(
        visible["result"]["view"],
        serde_json::to_value(durable.view).unwrap()
    );
    drop(initiator);
    fixture.shutdown().await;
}

#[tokio::test]
async fn discovery_is_concurrent_cancellable_and_shutdown_drains_session_work() {
    let mut upstream = HeldInspectionServer::start().await;
    let mut fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open-before-inspection",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
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
    let queued_push = tokio::time::timeout(Duration::from_millis(200), read_frame(&mut stream))
        .await
        .expect("open-target was blocked behind discovery")
        .unwrap();
    assert_eq!(queued_push["type"], "target-view");
    let opened = tokio::time::timeout(Duration::from_millis(200), read_frame(&mut stream))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(opened["requestId"], "open-while-held");
    assert_eq!(opened["result"]["kind"], "target-view");
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
async fn inspection_admission_is_bounded_and_reaped_capacity_is_reused() {
    let mut upstream = HeldInspectionServer::start().await;
    let mut fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;

    request(
        &mut stream,
        "open-before-admission",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    for index in 0..5 {
        write_frame(
            &mut stream,
            &json!({
                "type": "request",
                "requestId": format!("held-{index}"),
                "operation": {
                    "kind": "discover-models",
                    "target": "codex",
                    "source": {
                        "kind": "draft",
                        "baseUrl": upstream.base_url,
                        "authentication": "openai-bearer",
                        "credentialSource": {
                            "kind": "ephemeral",
                            "value": "admission-secret-must-not-escape"
                        }
                    }
                }
            }),
        )
        .await
        .unwrap();
    }
    for _ in 0..4 {
        upstream.wait_started().await;
    }

    let rejected = tokio::time::timeout(Duration::from_millis(250), read_frame(&mut stream))
        .await
        .expect("fifth inspection was admitted instead of rejected")
        .unwrap();
    assert_eq!(rejected["requestId"], "held-4");
    assert_eq!(rejected["problem"]["code"], "inspection-limit-reached");
    assert!(
        !rejected
            .to_string()
            .contains("admission-secret-must-not-escape")
    );
    assert_eq!(fixture.handle.as_ref().unwrap().tracked_inspections(), 4);

    write_frame(
        &mut stream,
        &json!({ "type": "cancel", "requestId": "held-0" }),
    )
    .await
    .unwrap();
    upstream.wait_dropped().await;
    wait_for_inspections(&fixture, 3).await;

    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "held-replacement",
            "operation": {
                "kind": "discover-models",
                "target": "codex",
                "source": {
                    "kind": "draft",
                    "baseUrl": upstream.base_url,
                    "authentication": "openai-bearer",
                    "credentialSource": {
                        "kind": "ephemeral",
                        "value": "replacement-secret-must-not-escape"
                    }
                }
            }
        }),
    )
    .await
    .unwrap();
    upstream.wait_started().await;
    wait_for_inspections(&fixture, 4).await;

    for request_id in ["held-1", "held-2", "held-3", "held-replacement"] {
        write_frame(
            &mut stream,
            &json!({ "type": "cancel", "requestId": request_id }),
        )
        .await
        .unwrap();
    }
    for _ in 0..4 {
        upstream.wait_dropped().await;
    }
    wait_for_zero_inspections(&fixture).await;

    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "held-shutdown",
            "operation": {
                "kind": "discover-models",
                "target": "codex",
                "source": {
                    "kind": "draft",
                    "baseUrl": upstream.base_url,
                    "authentication": "openai-bearer",
                    "credentialSource": {
                        "kind": "ephemeral",
                        "value": "shutdown-secret-must-not-escape"
                    }
                }
            }
        }),
    )
    .await
    .unwrap();
    upstream.wait_started().await;
    fixture.handle.as_mut().unwrap().request_shutdown();
    upstream.wait_dropped().await;
    wait_for_zero_inspections(&fixture).await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn completed_and_disconnected_inspections_are_reaped_without_orphans() {
    let completed_upstream = CompletedInspectionServer::start().await;
    let mut fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open-before-completed",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
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
    read_frame(&mut stream).await.unwrap();
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
    read_frame(&mut stream).await.unwrap();
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
async fn duplicate_inspection_id_closes_before_queued_frames_can_cross_correlate() {
    let mut upstream = CountingInspectionServer::start().await;
    let mut fixture = ControlFixture::start().await;
    fixture
        .store
        .apply_provider_action(
            Uuid::new_v4(),
            0,
            json!({
                "kind": "create-provider",
                "name": "Blocked writer",
                "baseUrl": "https://provider.example/v1",
                "model": "m".repeat(512 * 1024),
                "credential": { "kind": "replace", "value": "blocked-writer-secret" },
                "presetKey": null,
            }),
        )
        .await
        .unwrap();
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;

    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "block-writer",
            "operation": { "kind": "open-target", "target": "codex" }
        }),
    )
    .await
    .unwrap();
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "reused-inspection-id",
            "operation": {
                "kind": "discover-models",
                "target": "codex",
                "source": {
                    "kind": "draft",
                    "baseUrl": upstream.base_url,
                    "authentication": "openai-bearer",
                    "credentialSource": {
                        "kind": "ephemeral",
                        "value": "queued-result-secret-must-not-escape"
                    }
                }
            }
        }),
    )
    .await
    .unwrap();
    upstream.wait_completed().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    write_frame(
        &mut stream,
        &json!({ "type": "cancel", "requestId": "reused-inspection-id" }),
    )
    .await
    .unwrap();
    write_frame(
        &mut stream,
        &json!({
            "type": "request",
            "requestId": "reused-inspection-id",
            "operation": {
                "kind": "discover-models",
                "target": "codex",
                "source": {
                    "kind": "draft",
                    "baseUrl": upstream.base_url,
                    "authentication": "openai-bearer",
                    "credentialSource": {
                        "kind": "ephemeral",
                        "value": "second-secret-must-not-escape"
                    }
                }
            }
        }),
    )
    .await
    .unwrap();
    upstream.assert_no_completion().await;

    let blocked = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut stream))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blocked["requestId"], "block-writer");
    let original = read_frame(&mut stream).await.unwrap();
    assert_eq!(original["requestId"], "reused-inspection-id");
    assert_eq!(original["result"]["kind"], "model-discovery");
    let duplicate = read_frame(&mut stream).await.unwrap();
    assert_eq!(duplicate["requestId"], "reused-inspection-id");
    assert_eq!(duplicate["problem"]["code"], "request-in-progress");
    wait_for_zero_inspections(&fixture).await;
    let closed = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut stream))
        .await
        .expect("duplicate request ID did not close the session");
    assert!(closed.is_err());
    upstream.assert_no_completion().await;
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

#[tokio::test]
async fn claude_unknown_nonempty_session_opens_read_only_but_takeover_has_no_side_effects() {
    let root = short_temp_root("mx-claude-unknown");
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let saved = store
        .apply_provider_action_for(
            muxvia_routing::control::protocol::Target::Claude,
            Uuid::new_v4(),
            0,
            json!({
                "kind": "create-provider", "name": "Claude",
                "baseUrl": "https://api.anthropic.test", "model": "claude-test",
                "credential": {"kind": "replace", "value": "provider-secret"},
                "authentication": "anthropic-api-key", "presetKey": null
            }),
        )
        .await
        .unwrap();
    let provider_id = saved.view.providers[0].id;
    let before = saved.view;
    let codex_before = store.target_view().await.unwrap();
    let probe = Arc::new(CountingClaudeProbe(AtomicUsize::new(0)));
    let activation = Arc::new(
        ActivationService::new(
            Arc::clone(&store),
            home.clone(),
            Arc::new(ControlCodexProbe),
            "/usr/bin/codex".into(),
            Arc::new(ControlNoopUpstream),
        )
        .with_claude_runtime(probe.clone(), "/usr/bin/claude".into()),
    );
    let handle = ControlServer::bind_with_activation(
        &home,
        Arc::clone(&store),
        "routing-test",
        Arc::clone(&activation),
    )
    .await
    .unwrap();
    let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
    hello(&mut stream).await;
    let opened = request(
        &mut stream,
        "open",
        json!({
            "kind": "open-target", "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "unknown-nonempty",
                "blockingSelector": "CLAUDE_CODE_USE_VERTEX",
                "hostManagedState": "unmanaged",
                "cwd": user_home
            }
        }),
    )
    .await;
    assert_eq!(
        opened["result"]["view"],
        serde_json::to_value(&before).unwrap()
    );
    let action_id = Uuid::new_v4();

    let blocked = request(
        &mut stream,
        "activate",
        json!({
            "kind": "act", "target": "claude", "actionId": action_id,
            "expectedRevision": before.management_revision,
            "action": {
                "kind": "activate-provider", "providerId": provider_id,
                "mode": "takeover"
            }
        }),
    )
    .await;

    assert_eq!(blocked["problem"]["code"], "provider-mode-active");
    assert_eq!(blocked["problem"]["source"], "control-plane-context");
    assert_eq!(blocked["problem"]["selector"], "CLAUDE_CODE_USE_VERTEX");
    assert_eq!(
        blocked["authoritativeView"],
        serde_json::to_value(&before).unwrap()
    );
    assert_eq!(probe.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .target_view_for(muxvia_routing::control::protocol::Target::Claude)
            .await
            .unwrap(),
        before
    );
    assert_eq!(store.target_view().await.unwrap(), codex_before);
    assert!(
        store
            .receipt_for(muxvia_routing::control::protocol::Target::Claude, action_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .recovery_intent_for(muxvia_routing::control::protocol::Target::Claude, action_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .activated_snapshot_for(muxvia_routing::control::protocol::Target::Claude)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .routing_credential_for(muxvia_routing::control::protocol::Target::Claude)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        activation
            .model_endpoint_for(muxvia_routing::control::protocol::Target::Claude)
            .await
            .is_none()
    );
    assert!(!user_home.join(".claude/settings.json").exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), read_frame(&mut stream))
            .await
            .is_err(),
        "a pre-side-effect blocker published a Target View"
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn unmanaged_unknown_compatible_activation_requires_exact_ack_for_both_targets_and_modes() {
    const SECRETS: &[&str] = &[
        "UNKNOWN_CODEX_SECRET_98101",
        "UNKNOWN_CLAUDE_SECRET_98102",
        "UNKNOWN_COMPATIBILITY_CONFIG_SENTINEL_98103",
        "UNKNOWN_COMPATIBILITY_BACKEND_SENTINEL_98104",
        "UNKNOWN_COMPATIBILITY_SETTINGS_SENTINEL_98105",
    ];
    for (target, target_name, mode) in [
        (Target::Codex, "codex", "direct"),
        (Target::Codex, "codex", "takeover"),
        (Target::Claude, "claude", "direct"),
        (Target::Claude, "claude", "takeover"),
    ] {
        let root = short_temp_root(&format!("mx-unknown-{target_name}-{mode}"));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let saved = store
            .apply_provider_action_for(
                target,
                Uuid::new_v4(),
                0,
                match target {
                    Target::Codex => json!({
                        "kind": "create-provider", "name": "Codex unknown",
                        "baseUrl": "https://api.openai.test/v1", "model": "gpt-test",
                        "credential": {"kind": "replace", "value": "UNKNOWN_CODEX_SECRET_98101"},
                        "authentication": "openai-bearer", "presetKey": null
                    }),
                    Target::Claude => json!({
                        "kind": "create-provider", "name": "Claude unknown",
                        "baseUrl": "https://api.anthropic.test", "model": "claude-test",
                        "credential": {"kind": "replace", "value": "UNKNOWN_CLAUDE_SECRET_98102"},
                        "authentication": "anthropic-api-key", "presetKey": null
                    }),
                },
            )
            .await
            .unwrap();
        let provider_id = saved.view.providers[0].id;
        let peer_target = match target {
            Target::Codex => Target::Claude,
            Target::Claude => Target::Codex,
        };
        let peer_before_value =
            serde_json::to_value(store.target_view_for(peer_target).await.unwrap()).unwrap();
        assert_compatibility_wire_is_secret_free(
            &peer_before_value,
            SECRETS,
            "unknown-peer-before",
        );
        let peer_before = serde_json::to_vec(&peer_before_value).unwrap();
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(UnknownCodexProbe("codex-unknown-8.1")),
                "/usr/bin/codex".into(),
                Arc::new(ControlNoopUpstream),
            )
            .with_claude_runtime(
                Arc::new(UnknownClaudeProbe("claude-unknown-8.1")),
                "/usr/bin/claude".into(),
            ),
        );
        let handle = ControlServer::bind_with_activation(
            &home,
            Arc::clone(&store),
            "routing-test",
            activation,
        )
        .await
        .unwrap();
        let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut stream).await;
        let claude_context = (target == Target::Claude).then(|| {
            json!({
                "claudeConfigDir": null,
                "selectorState": "unset",
                "blockingSelector": null,
                "hostManagedState": "unmanaged",
                "cwd": user_home
            })
        });
        let opened = request(
            &mut stream,
            "open",
            json!({
                "kind": "open-target", "target": target_name,
                "claudeContext": claude_context
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&opened, SECRETS, "unknown-open");
        let revision = opened["result"]["view"]["managementRevision"]
            .as_u64()
            .unwrap();

        let blocked = request(
            &mut stream,
            "blocked-activation",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": revision,
                "action": {
                    "kind": "activate-provider", "providerId": provider_id, "mode": mode,
                    "configDiagnostic": "UNKNOWN_COMPATIBILITY_CONFIG_SENTINEL_98103",
                    "backendDiagnostic": "UNKNOWN_COMPATIBILITY_BACKEND_SENTINEL_98104",
                    "settingsDiagnostic": "UNKNOWN_COMPATIBILITY_SETTINGS_SENTINEL_98105"
                }
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&blocked, SECRETS, "unknown-blocked");
        assert_eq!(
            blocked["problem"]["code"],
            "compatibility-acknowledgement-required"
        );
        let projected = read_frame(&mut stream).await.unwrap();
        assert_compatibility_wire_is_secret_free(&projected, SECRETS, "unknown-projected");
        assert_eq!(projected["type"], "target-view");
        assert!(
            projected["view"]["problems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|problem| { problem["code"] == "compatibility-acknowledgement-required" })
        );
        let version = format!("{target_name}-unknown-8.1");
        let compatibility = store.compatibility_for(target).await.unwrap();
        assert_eq!(compatibility.version, version);
        assert_eq!(
            compatibility.classification,
            CompatibilityClassification::UnknownCompatible
        );
        assert!(compatibility.acknowledgement_required);
        let peer_after_value =
            serde_json::to_value(store.target_view_for(peer_target).await.unwrap()).unwrap();
        assert_compatibility_wire_is_secret_free(&peer_after_value, SECRETS, "unknown-peer-after");
        let peer_after = serde_json::to_vec(&peer_after_value).unwrap();
        assert!(
            peer_after == peer_before,
            "compatibility blocking changed the peer Target view"
        );
        assert!(
            !user_home
                .join(format!(
                    ".{target_name}/{}",
                    if target == Target::Codex {
                        "config.toml"
                    } else {
                        "settings.json"
                    }
                ))
                .exists()
        );

        let preview = request(
            &mut stream,
            "compatibility-probe",
            json!({ "kind": "probe-compatibility", "target": target_name }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&preview, SECRETS, "unknown-probe");
        assert_eq!(preview["type"], "response");
        assert_eq!(preview["result"]["kind"], "compatibility-probe");
        assert_eq!(preview["result"]["probe"]["target"], target_name);
        assert_eq!(
            preview["result"]["probe"]["compatibility"]["version"],
            version
        );
        assert_eq!(
            preview["result"]["probe"]["compatibility"]["classification"],
            "unknown-compatible"
        );
        assert_eq!(
            preview["result"]["probe"]["compatibility"]["acknowledgementRequired"],
            true
        );
        let acknowledgement_action_id = Uuid::new_v4();
        let acknowledged = request(
            &mut stream,
            "compatibility-acknowledgement",
            json!({
                "kind": "act", "target": target_name, "actionId": acknowledgement_action_id,
                "expectedRevision": revision,
                "action": { "kind": "resolve-compatibility", "version": version }
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&acknowledged, SECRETS, "unknown-resolved");
        assert_eq!(acknowledged["type"], "response");
        assert_eq!(acknowledged["result"]["outcome"]["status"], "applied");
        assert!(
            !acknowledged["result"]["outcome"]["view"]["problems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|problem| problem["code"] == "compatibility-acknowledgement-required")
        );
        let acknowledgement_push = read_frame(&mut stream).await.unwrap();
        assert_compatibility_wire_is_secret_free(
            &acknowledgement_push,
            SECRETS,
            "unknown-resolution-push",
        );
        assert_eq!(acknowledgement_push["type"], "target-view");
        let replayed = request(
            &mut stream,
            "compatibility-acknowledgement-replay",
            json!({
                "kind": "act", "target": target_name, "actionId": acknowledgement_action_id,
                "expectedRevision": revision + 99,
                "action": { "kind": "resolve-compatibility", "version": "stale-version" }
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&replayed, SECRETS, "unknown-replay");
        assert_eq!(replayed["result"]["outcome"]["status"], "replayed");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), read_frame(&mut stream))
                .await
                .is_err(),
            "a compatibility acknowledgement replay published a duplicate Target View"
        );
        let activated = request(
            &mut stream,
            "acknowledged-activation",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": revision,
                "action": {
                    "kind": "activate-provider", "providerId": provider_id, "mode": mode,
                    "configDiagnostic": "UNKNOWN_COMPATIBILITY_CONFIG_SENTINEL_98103",
                    "backendDiagnostic": "UNKNOWN_COMPATIBILITY_BACKEND_SENTINEL_98104",
                    "settingsDiagnostic": "UNKNOWN_COMPATIBILITY_SETTINGS_SENTINEL_98105"
                }
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&activated, SECRETS, "unknown-activation");
        assert_eq!(activated["type"], "response");
        assert_eq!(activated["result"]["outcome"]["status"], "applied");
        assert!(
            !activated["result"]["outcome"]["view"]["problems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|problem| problem["code"] == "compatibility-acknowledgement-required")
        );
        let activation_push = read_frame(&mut stream).await.unwrap();
        assert_compatibility_wire_is_secret_free(
            &activation_push,
            SECRETS,
            "unknown-activation-push",
        );
        handle.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn tested_probe_resolves_stale_unknown_and_incompatible_blockers_for_both_targets() {
    const SECRETS: &[&str] = &[
        "TESTED_RESOLUTION_CODEX_SECRET_98701",
        "TESTED_RESOLUTION_CLAUDE_SECRET_98702",
        "TESTED_RESOLUTION_CONFIG_SENTINEL_98703",
        "TESTED_RESOLUTION_BACKEND_SENTINEL_98704",
        "TESTED_RESOLUTION_SETTINGS_SENTINEL_98705",
    ];
    for (target, target_name, initial_probe_state) in [
        (Target::Codex, "codex", 0),
        (Target::Codex, "codex", 1),
        (Target::Claude, "claude", 0),
        (Target::Claude, "claude", 1),
    ] {
        let root = short_temp_root(&format!(
            "mx-tested-resolution-{target_name}-{initial_probe_state}"
        ));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let saved = store
            .apply_provider_action_for(
                target,
                Uuid::new_v4(),
                0,
                match target {
                    Target::Codex => json!({
                        "kind": "create-provider", "name": "Codex changing",
                        "baseUrl": "https://api.openai.test/v1", "model": "gpt-test",
                        "credential": {"kind": "replace", "value": "TESTED_RESOLUTION_CODEX_SECRET_98701"},
                        "authentication": "openai-bearer", "presetKey": null
                    }),
                    Target::Claude => json!({
                        "kind": "create-provider", "name": "Claude changing",
                        "baseUrl": "https://api.anthropic.test", "model": "claude-test",
                        "credential": {"kind": "replace", "value": "TESTED_RESOLUTION_CLAUDE_SECRET_98702"},
                        "authentication": "anthropic-api-key", "presetKey": null
                    }),
                },
            )
            .await
            .unwrap();
        let provider_id = saved.view.providers[0].id;
        let probe_state = Arc::new(AtomicUsize::new(initial_probe_state));
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(ChangingCodexProbe {
                    state: Arc::clone(&probe_state),
                    calls: None,
                }),
                "/usr/bin/codex".into(),
                Arc::new(ControlNoopUpstream),
            )
            .with_claude_runtime(
                Arc::new(ChangingClaudeProbe {
                    state: Arc::clone(&probe_state),
                    calls: None,
                }),
                "/usr/bin/claude".into(),
            ),
        );
        let handle = ControlServer::bind_with_activation(
            &home,
            Arc::clone(&store),
            "routing-test",
            activation,
        )
        .await
        .unwrap();
        let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut stream).await;
        let claude_context = (target == Target::Claude).then(|| {
            json!({
                "claudeConfigDir": null,
                "selectorState": "unset",
                "blockingSelector": null,
                "hostManagedState": "unmanaged",
                "cwd": user_home
            })
        });
        let opened = request(
            &mut stream,
            "open",
            json!({
                "kind": "open-target", "target": target_name,
                "claudeContext": claude_context
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&opened, SECRETS, "tested-open");
        let revision = opened["result"]["view"]["managementRevision"]
            .as_u64()
            .unwrap();
        let blocked = request(
            &mut stream,
            "stale-compatibility-blocker",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": revision,
                "action": {
                    "kind": "activate-provider", "providerId": provider_id, "mode": "direct",
                    "configDiagnostic": "TESTED_RESOLUTION_CONFIG_SENTINEL_98703",
                    "backendDiagnostic": "TESTED_RESOLUTION_BACKEND_SENTINEL_98704",
                    "settingsDiagnostic": "TESTED_RESOLUTION_SETTINGS_SENTINEL_98705"
                }
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&blocked, SECRETS, "tested-blocked");
        assert_eq!(
            blocked["problem"]["code"],
            if initial_probe_state == 0 {
                "compatibility-acknowledgement-required"
            } else {
                "incompatible-target-cli"
            }
        );
        let blocker_push = read_frame(&mut stream).await.unwrap();
        assert_compatibility_wire_is_secret_free(&blocker_push, SECRETS, "tested-blocker-push");
        store
            .record_startup_problem_for(
                target,
                "startup-reconciliation-failed",
                "Unrelated startup problem",
            )
            .await
            .unwrap();
        probe_state.store(2, Ordering::SeqCst);

        let probe = request(
            &mut stream,
            "tested-compatibility-probe",
            json!({"kind": "probe-compatibility", "target": target_name}),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&probe, SECRETS, "tested-probe");
        let tested_version = format!("{target_name}-tested-8.2");
        assert_eq!(
            probe["result"]["probe"]["compatibility"],
            json!({
                "version": tested_version,
                "classification": "tested",
                "acknowledgementRequired": false
            })
        );
        let resolution_action_id = Uuid::new_v4();
        let resolved = request(
            &mut stream,
            "tested-compatibility-resolution",
            json!({
                "kind": "act", "target": target_name,
                "actionId": resolution_action_id,
                "expectedRevision": revision,
                "action": {"kind": "resolve-compatibility", "version": tested_version}
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&resolved, SECRETS, "tested-resolved");
        assert_eq!(resolved["result"]["outcome"]["status"], "applied");
        let problems = resolved["result"]["outcome"]["view"]["problems"]
            .as_array()
            .unwrap();
        assert!(
            problems
                .iter()
                .any(|problem| { problem["code"] == "startup-reconciliation-failed" })
        );
        assert!(!problems.iter().any(|problem| {
            matches!(
                problem["code"].as_str(),
                Some("compatibility-acknowledgement-required" | "incompatible-target-cli")
            )
        }));
        let resolution_push = read_frame(&mut stream).await.unwrap();
        assert_compatibility_wire_is_secret_free(
            &resolution_push,
            SECRETS,
            "tested-resolution-push",
        );
        assert_eq!(resolution_push["type"], "target-view");
        let compatibility = store.compatibility_for(target).await.unwrap();
        assert_eq!(compatibility.version, tested_version);
        assert_eq!(
            compatibility.classification,
            CompatibilityClassification::Tested
        );
        assert!(!compatibility.acknowledgement_required);

        let replayed = request(
            &mut stream,
            "tested-compatibility-resolution-replay",
            json!({
                "kind": "act", "target": target_name,
                "actionId": resolution_action_id,
                "expectedRevision": revision + 99,
                "action": {"kind": "resolve-compatibility", "version": "stale-version"}
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&replayed, SECRETS, "tested-replay");
        assert_eq!(replayed["result"]["outcome"]["status"], "replayed");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), read_frame(&mut stream))
                .await
                .is_err(),
            "a compatibility resolution replay published a duplicate Target View"
        );
        handle.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn compatibility_resolution_binds_the_probed_revision_across_two_real_sessions() {
    const SECRETS: &[&str] = &[
        "REVISION_RACE_CODEX_CREDENTIAL_98801",
        "REVISION_RACE_CLAUDE_CREDENTIAL_98802",
        "REVISION_RACE_CONFIG_SENTINEL_98803",
        "REVISION_RACE_BACKEND_SENTINEL_98804",
        "REVISION_RACE_SETTINGS_SENTINEL_98805",
    ];
    for (target, target_name) in [(Target::Codex, "codex"), (Target::Claude, "claude")] {
        let root = short_temp_root(&format!("mx-compatibility-revision-{target_name}"));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let probe_state = Arc::new(AtomicUsize::new(0));
        let probe_calls = Arc::new(AtomicUsize::new(0));
        let managed_path = user_home.join(match target {
            Target::Codex => ".codex/config.toml",
            Target::Claude => ".claude/settings.json",
        });
        fs::create_dir_all(managed_path.parent().unwrap()).unwrap();
        fs::write(
            &managed_path,
            match target {
                Target::Codex => "unrelated = \"REVISION_RACE_CONFIG_SENTINEL_98803\"\n",
                Target::Claude => "{\"unrelated\":\"REVISION_RACE_SETTINGS_SENTINEL_98805\"}\n",
            },
        )
        .unwrap();
        let managed_before = secret_file_fingerprint(&managed_path);
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(ChangingCodexProbe {
                    state: Arc::clone(&probe_state),
                    calls: Some(Arc::clone(&probe_calls)),
                }),
                "/usr/bin/codex".into(),
                Arc::new(ControlNoopUpstream),
            )
            .with_claude_runtime(
                Arc::new(ChangingClaudeProbe {
                    state: Arc::clone(&probe_state),
                    calls: Some(Arc::clone(&probe_calls)),
                }),
                "/usr/bin/claude".into(),
            ),
        );
        let handle = ControlServer::bind_with_activation(
            &home,
            Arc::clone(&store),
            "routing-test",
            Arc::clone(&activation),
        )
        .await
        .unwrap();
        let mut first = UnixStream::connect(handle.socket_path()).await.unwrap();
        let mut second = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut first).await;
        hello(&mut second).await;
        let claude_context = (target == Target::Claude).then(|| {
            json!({
                "claudeConfigDir": null, "selectorState": "unset",
                "blockingSelector": null, "hostManagedState": "unmanaged", "cwd": user_home
            })
        });
        for (stream, request_id) in [(&mut first, "open-first"), (&mut second, "open-second")] {
            let opened = request(
                stream,
                request_id,
                json!({
                    "kind": "open-target", "target": target_name,
                    "claudeContext": claude_context
                }),
            )
            .await;
            assert_compatibility_wire_is_secret_free(&opened, SECRETS, "revision-race-open");
            assert_eq!(opened["result"]["view"]["managementRevision"], 0);
        }

        let first_probe = request(
            &mut first,
            "probe-at-zero",
            json!({"kind": "probe-compatibility", "target": target_name}),
        )
        .await;
        assert_compatibility_wire_is_secret_free(
            &first_probe,
            SECRETS,
            "revision-race-first-probe",
        );
        assert_eq!(first_probe["result"]["probe"]["managementRevision"], 0);
        let unknown_version = format!("{target_name}-stale-unknown");
        assert_eq!(
            first_probe["result"]["probe"]["compatibility"]["version"],
            unknown_version
        );

        probe_state.store(2, Ordering::SeqCst);
        let committed = request(
            &mut second,
            "peer-commit",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": 0,
                "action": match target {
                    Target::Codex => json!({
                        "kind": "create-provider", "name": "Codex peer", "baseUrl": "https://api.openai.test/v1",
                        "model": "gpt-test", "credential": {"kind": "replace", "value": "REVISION_RACE_CODEX_CREDENTIAL_98801"},
                        "authentication": "openai-bearer", "presetKey": null,
                        "configDiagnostic": "REVISION_RACE_CONFIG_SENTINEL_98803",
                        "backendDiagnostic": "REVISION_RACE_BACKEND_SENTINEL_98804",
                        "settingsDiagnostic": "REVISION_RACE_SETTINGS_SENTINEL_98805"
                    }),
                    Target::Claude => json!({
                        "kind": "create-provider", "name": "Claude peer", "baseUrl": "https://api.anthropic.test",
                        "model": "claude-test", "credential": {"kind": "replace", "value": "REVISION_RACE_CLAUDE_CREDENTIAL_98802"},
                        "authentication": "anthropic-api-key", "presetKey": null,
                        "configDiagnostic": "REVISION_RACE_CONFIG_SENTINEL_98803",
                        "backendDiagnostic": "REVISION_RACE_BACKEND_SENTINEL_98804",
                        "settingsDiagnostic": "REVISION_RACE_SETTINGS_SENTINEL_98805"
                    }),
                }
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&committed, SECRETS, "revision-race-peer-commit");
        assert_eq!(
            committed["result"]["outcome"]["view"]["managementRevision"],
            1
        );
        for (stream, label) in [
            (&mut first, "revision-race-first-peer-push"),
            (&mut second, "revision-race-second-peer-push"),
        ] {
            let push = read_frame(stream).await.unwrap();
            assert_compatibility_wire_is_secret_free(&push, SECRETS, label);
            assert_eq!(push["view"]["managementRevision"], 1);
        }
        let before_stale =
            serde_json::to_value(store.target_view_for(target).await.unwrap()).unwrap();
        let compatibility_before_missing = store.compatibility_for(target).await.is_err();
        let runtime_before = activation.model_endpoint_for(target).await;
        for (cli_state, request_id) in [
            (1, "resolve-stale-after-incompatible"),
            (2, "resolve-stale-after-version-change"),
        ] {
            probe_state.store(cli_state, Ordering::SeqCst);
            let calls_before = probe_calls.load(Ordering::SeqCst);
            let action_id = Uuid::new_v4();
            let stale = request(
                &mut first,
                request_id,
                json!({
                    "kind": "act", "target": target_name, "actionId": action_id,
                    "expectedRevision": 0,
                    "action": {"kind": "resolve-compatibility", "version": unknown_version}
                }),
            )
            .await;
            assert_compatibility_wire_is_secret_free(
                &stale,
                SECRETS,
                "revision-race-stale-response",
            );
            assert_eq!(stale["problem"]["code"], "stale-revision");
            let after_stale =
                serde_json::to_value(store.target_view_for(target).await.unwrap()).unwrap();
            let compatibility_after_missing = store.compatibility_for(target).await.is_err();
            assert_compatibility_json_equal(
                &after_stale,
                &before_stale,
                SECRETS,
                "revision-race-target-view",
            );
            assert!(compatibility_before_missing && compatibility_after_missing);
            assert!(
                store
                    .receipt_for(target, action_id)
                    .await
                    .unwrap()
                    .is_none(),
                "stale compatibility resolution wrote a receipt"
            );
            assert_eq!(
                probe_calls.load(Ordering::SeqCst),
                calls_before,
                "stale compatibility resolution re-probed the Target CLI"
            );
            assert_secret_file_unchanged(&managed_path, &managed_before);
            assert_eq!(activation.model_endpoint_for(target).await, runtime_before);
            for stream in [&mut first, &mut second] {
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), read_frame(stream))
                        .await
                        .is_err(),
                    "stale compatibility resolution published a Target View"
                );
            }
        }

        probe_state.store(0, Ordering::SeqCst);
        let fresh_probe = request(
            &mut first,
            "probe-at-one",
            json!({"kind": "probe-compatibility", "target": target_name}),
        )
        .await;
        assert_compatibility_wire_is_secret_free(
            &fresh_probe,
            SECRETS,
            "revision-race-fresh-probe",
        );
        assert_eq!(fresh_probe["result"]["probe"]["managementRevision"], 1);
        let resolved = request(
            &mut first,
            "resolve-fresh-probe",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": 1,
                "action": {"kind": "resolve-compatibility", "version": unknown_version}
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&resolved, SECRETS, "revision-race-resolved");
        assert_eq!(resolved["result"]["outcome"]["status"], "applied");
        for (stream, label) in [
            (&mut first, "revision-race-first-resolution-push"),
            (&mut second, "revision-race-second-resolution-push"),
        ] {
            let push = read_frame(stream).await.unwrap();
            assert_compatibility_wire_is_secret_free(&push, SECRETS, label);
        }
        let compatibility = store.compatibility_for(target).await.unwrap();
        assert_eq!(compatibility.version, unknown_version);
        assert_eq!(
            compatibility.classification,
            CompatibilityClassification::UnknownCompatible
        );
        assert!(!compatibility.acknowledgement_required);

        handle.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn unmanaged_incompatible_activation_exposes_only_public_read_only_guidance() {
    const SECRETS: &[&str] = &[
        "INCOMPATIBLE_CODEX_SECRET_98501",
        "INCOMPATIBLE_CLAUDE_SECRET_98502",
        "INCOMPATIBLE_CONFIG_SENTINEL_98503",
        "INCOMPATIBLE_BACKEND_SENTINEL_98504",
        "INCOMPATIBLE_SETTINGS_SENTINEL_98505",
    ];
    for (target, target_name) in [(Target::Codex, "codex"), (Target::Claude, "claude")] {
        let root = short_temp_root(&format!("mx-incompatible-{target_name}"));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let saved = store
            .apply_provider_action_for(
                target,
                Uuid::new_v4(),
                0,
                match target {
                    Target::Codex => json!({
                        "kind": "create-provider", "name": "Codex incompatible",
                        "baseUrl": "https://api.openai.test/v1", "model": "gpt-test",
                        "credential": {"kind": "replace", "value": "INCOMPATIBLE_CODEX_SECRET_98501"},
                        "authentication": "openai-bearer", "presetKey": null
                    }),
                    Target::Claude => json!({
                        "kind": "create-provider", "name": "Claude incompatible",
                        "baseUrl": "https://api.anthropic.test", "model": "claude-test",
                        "credential": {"kind": "replace", "value": "INCOMPATIBLE_CLAUDE_SECRET_98502"},
                        "authentication": "anthropic-api-key", "presetKey": null
                    }),
                },
            )
            .await
            .unwrap();
        let provider_id = saved.view.providers[0].id;
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(IncompatibleCodexProbe),
                "/usr/bin/codex".into(),
                Arc::new(ControlNoopUpstream),
            )
            .with_claude_runtime(Arc::new(IncompatibleClaudeProbe), "/usr/bin/claude".into()),
        );
        let handle = ControlServer::bind_with_activation(
            &home,
            Arc::clone(&store),
            "routing-test",
            activation,
        )
        .await
        .unwrap();
        let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut stream).await;
        let claude_context = (target == Target::Claude).then(|| {
            json!({
                "claudeConfigDir": null, "selectorState": "unset",
                "blockingSelector": null, "hostManagedState": "unmanaged", "cwd": user_home
            })
        });
        let opened = request(
            &mut stream,
            "open",
            json!({
                "kind": "open-target", "target": target_name,
                "claudeContext": claude_context
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&opened, SECRETS, "incompatible-open");
        let revision = opened["result"]["view"]["managementRevision"]
            .as_u64()
            .unwrap();
        let blocked = request(
            &mut stream,
            "blocked-activation",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": revision,
                "action": {
                    "kind": "activate-provider", "providerId": provider_id, "mode": "direct",
                    "configDiagnostic": "INCOMPATIBLE_CONFIG_SENTINEL_98503",
                    "backendDiagnostic": "INCOMPATIBLE_BACKEND_SENTINEL_98504",
                    "settingsDiagnostic": "INCOMPATIBLE_SETTINGS_SENTINEL_98505"
                }
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&blocked, SECRETS, "incompatible-blocked");
        assert_eq!(blocked["problem"]["code"], "incompatible-target-cli");
        let blocker_push = read_frame(&mut stream).await.unwrap();
        assert_compatibility_wire_is_secret_free(&blocker_push, SECRETS, "incompatible-push");
        assert_eq!(blocker_push["type"], "target-view");

        let preview = request(
            &mut stream,
            "compatibility-probe",
            json!({"kind": "probe-compatibility", "target": target_name}),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&preview, SECRETS, "incompatible-probe");
        assert_eq!(preview["type"], "response");
        assert_eq!(
            preview["result"]["probe"]["compatibility"]["classification"],
            "incompatible"
        );
        assert_eq!(
            preview["result"]["probe"]["compatibility"]["version"],
            "unavailable"
        );
        let rejected_resolution = request(
            &mut stream,
            "incompatible-resolution",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": revision,
                "action": {"kind": "resolve-compatibility", "version": "unavailable"}
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(
            &rejected_resolution,
            SECRETS,
            "incompatible-resolution-rejection",
        );
        assert_eq!(
            rejected_resolution["problem"]["code"],
            "incompatible-target-cli"
        );
        assert!(
            !user_home
                .join(format!(
                    ".{target_name}/{}",
                    if target == Target::Codex {
                        "config.toml"
                    } else {
                        "settings.json"
                    }
                ))
                .exists()
        );
        handle.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn durable_claude_shadow_problem_retains_every_exact_selector_across_reopen() {
    const SECRETS: &[&str] = &[
        "SELECTOR_SECRET_98601",
        "BLOCKED_SELECTOR_SECRET_98602",
        "SELECTOR_CONFIG_SENTINEL_98603",
        "SELECTOR_BACKEND_SENTINEL_98604",
        "SELECTOR_SETTINGS_SENTINEL_98605",
    ];
    for selector in [
        ClaudeBlockingSelector::Bedrock,
        ClaudeBlockingSelector::Vertex,
        ClaudeBlockingSelector::Foundry,
        ClaudeBlockingSelector::Mantle,
        ClaudeBlockingSelector::AnthropicAws,
        ClaudeBlockingSelector::HostManaged,
    ] {
        let root = short_temp_root(&format!("mx-selector-{}", selector.as_str().len()));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let saved = store
            .apply_provider_action_for(
                Target::Claude,
                Uuid::new_v4(),
                0,
                json!({
                    "kind": "create-provider", "name": "Claude managed",
                    "baseUrl": "https://api.anthropic.test", "model": "claude-test",
                    "credential": {"kind": "replace", "value": "SELECTOR_SECRET_98601"},
                    "authentication": "anthropic-api-key", "presetKey": null
                }),
            )
            .await
            .unwrap();
        let provider_id = saved.view.providers[0].id;
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(ControlCodexProbe),
                "/usr/bin/codex".into(),
                Arc::new(ControlNoopUpstream),
            )
            .with_claude_runtime(Arc::new(ControlClaudeProbe), "/usr/bin/claude".into()),
        );
        activation
            .apply_raw_for_with_context(
                Target::Claude,
                Uuid::new_v4(),
                saved.view.management_revision,
                json!({
                    "kind": "activate-provider", "providerId": provider_id, "mode": "direct"
                }),
                Some(&ClaudePreflightContext {
                    claude_config_dir: None,
                    selector_state: ClaudeSelectorState::Unset,
                    blocking_selector: None,
                    host_managed_state: ClaudeHostManagedState::Unmanaged,
                    cwd: user_home.to_string_lossy().into_owned(),
                }),
            )
            .await
            .unwrap();
        let handle = ControlServer::bind_with_activation(
            &home,
            Arc::clone(&store),
            "routing-test",
            activation,
        )
        .await
        .unwrap();
        let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut stream).await;
        let host_managed = selector == ClaudeBlockingSelector::HostManaged;
        let opened = request(
            &mut stream,
            "open",
            json!({
                "kind": "open-target", "target": "claude",
                "claudeContext": {
                    "claudeConfigDir": null,
                    "selectorState": if host_managed { "unset" } else { "enabled" },
                    "blockingSelector": selector,
                    "hostManagedState": if host_managed { "managed" } else { "unmanaged" },
                    "cwd": user_home
                }
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&opened, SECRETS, "selector-open");
        let revision = opened["result"]["view"]["managementRevision"]
            .as_u64()
            .unwrap();
        let blocked = request(
            &mut stream,
            "shadowed-write",
            json!({
                "kind": "act", "target": "claude", "actionId": Uuid::new_v4(),
                "expectedRevision": revision,
                "action": {
                    "kind": "create-provider", "name": "blocked", "baseUrl": "https://blocked.test",
                    "model": "blocked", "credential": {"kind": "replace", "value": "BLOCKED_SELECTOR_SECRET_98602"},
                    "authentication": "anthropic-api-key", "presetKey": null,
                    "configDiagnostic": "SELECTOR_CONFIG_SENTINEL_98603",
                    "backendDiagnostic": "SELECTOR_BACKEND_SENTINEL_98604",
                    "settingsDiagnostic": "SELECTOR_SETTINGS_SENTINEL_98605"
                }
            }),
        )
        .await;
        assert_compatibility_wire_is_secret_free(&blocked, SECRETS, "selector-blocked");
        assert_eq!(blocked["problem"]["code"], "shadowing-configuration");
        assert_eq!(
            blocked["problem"]["source"],
            if host_managed {
                "claude-host-managed"
            } else {
                "claude-selector"
            }
        );
        assert_eq!(blocked["problem"]["selector"], selector.as_str());
        let push = read_frame(&mut stream).await.unwrap();
        assert_compatibility_wire_is_secret_free(&push, SECRETS, "selector-push");
        let pushed_problem = push["view"]["problems"]
            .as_array()
            .unwrap()
            .iter()
            .find(|problem| problem["code"] == "shadowing-configuration")
            .unwrap();
        assert_eq!(pushed_problem["selector"], selector.as_str());
        handle.shutdown().await.unwrap();

        let reopened = StateStore::open(&home).await.unwrap();
        let reopened_view = reopened.target_view_for(Target::Claude).await.unwrap();
        assert_compatibility_wire_is_secret_free(
            &serde_json::to_value(&reopened_view).unwrap(),
            SECRETS,
            "selector-reopened",
        );
        let persisted = reopened_view
            .problems
            .into_iter()
            .find(|problem| problem.code == "shadowing-configuration")
            .unwrap();
        assert_eq!(persisted.selector, Some(selector));
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn reconciliation_adopt_persists_codec_canonical_path_for_directory_symlinks() {
    for (target, target_name, directory, file_name) in [
        (Target::Codex, "codex", ".codex", "config.toml"),
        (Target::Claude, "claude", ".claude", "settings.json"),
    ] {
        let root = short_temp_root(&format!("mx-canonical-{target_name}"));
        let user_home = root.join("home");
        let canonical_home = root.join(format!("canonical-{target_name}"));
        fs::create_dir_all(&user_home).unwrap();
        fs::create_dir(&canonical_home).unwrap();
        symlink(&canonical_home, user_home.join(directory)).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let activation = Arc::new(
            ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                Arc::new(ControlCodexProbe),
                "/usr/bin/codex".into(),
                Arc::new(ControlNoopUpstream),
            )
            .with_claude_runtime(Arc::new(ControlClaudeProbe), "/usr/bin/claude".into()),
        );
        let handle = ControlServer::bind_with_activation(
            &home,
            Arc::clone(&store),
            "routing-test",
            activation,
        )
        .await
        .unwrap();
        let mut stream = UnixStream::connect(handle.socket_path()).await.unwrap();
        hello(&mut stream).await;
        let claude_context = (target == Target::Claude).then(|| {
            json!({
                "claudeConfigDir": null,
                "selectorState": "unset",
                "blockingSelector": null,
                "hostManagedState": "unmanaged",
                "cwd": user_home
            })
        });
        let opened = request(
            &mut stream,
            "open",
            json!({
                "kind": "open-target", "target": target_name,
                "claudeContext": claude_context
            }),
        )
        .await;
        let saved = request(
            &mut stream,
            "save",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": opened["result"]["view"]["managementRevision"],
                "action": match target {
                    Target::Codex => json!({
                        "kind": "create-provider", "name": "Canonical Codex",
                        "baseUrl": "https://api.openai.test/v1", "model": "gpt-test",
                        "credential": {"kind": "replace", "value": "CANONICAL_CODEX_SECRET_98201"},
                        "authentication": "openai-bearer", "presetKey": null
                    }),
                    Target::Claude => json!({
                        "kind": "create-provider", "name": "Canonical Claude",
                        "baseUrl": "https://api.anthropic.test", "model": "claude-test",
                        "credential": {"kind": "replace", "value": "CANONICAL_CLAUDE_SECRET_98202"},
                        "authentication": "anthropic-api-key", "presetKey": null
                    }),
                }
            }),
        )
        .await;
        let _save_push = read_frame(&mut stream).await.unwrap();
        let activated = request(
            &mut stream,
            "activate",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": saved["result"]["outcome"]["view"]["managementRevision"],
                "action": {
                    "kind": "activate-provider",
                    "providerId": saved["result"]["outcome"]["view"]["providers"][0]["id"],
                    "mode": "direct"
                }
            }),
        )
        .await;
        let _activation_push = read_frame(&mut stream).await.unwrap();
        assert_eq!(activated["type"], "response");
        let canonical_path = fs::canonicalize(&canonical_home).unwrap().join(file_name);
        match target {
            Target::Codex => fs::write(
                &canonical_path,
                r#"model = "external-model"
model_provider = "external"
[model_providers.external]
name = "External"
base_url = "https://external.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer EXTERNAL_CODEX_SECRET_98203" }
supports_websockets = false
"#,
            )
            .unwrap(),
            Target::Claude => fs::write(
                &canonical_path,
                serde_json::to_vec_pretty(&json!({
                    "env": {
                        "ANTHROPIC_BASE_URL": "https://external.example",
                        "ANTHROPIC_MODEL": "external-model",
                        "ANTHROPIC_API_KEY": "EXTERNAL_CLAUDE_SECRET_98204"
                    }
                }))
                .unwrap(),
            )
            .unwrap(),
        }
        let preview = request(
            &mut stream,
            "preview",
            json!({
                "kind": "preview-reconciliation", "target": target_name, "strategy": "adopt"
            }),
        )
        .await;
        let adopted = request(
            &mut stream,
            "adopt",
            json!({
                "kind": "act", "target": target_name, "actionId": Uuid::new_v4(),
                "expectedRevision": preview["result"]["preview"]["managementRevision"],
                "action": {
                    "kind": "reconcile", "strategy": "adopt",
                    "observationToken": preview["result"]["preview"]["observationToken"]
                }
            }),
        )
        .await;
        assert_eq!(adopted["type"], "response");
        assert_eq!(
            adopted["result"]["outcome"]["view"]["managedConfiguration"]["path"],
            canonical_path.to_string_lossy().as_ref()
        );
        let recovery_id = Uuid::parse_str(
            adopted["result"]["outcome"]["view"]["recovery"]["intentId"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let recovery = store.recovery_intent(recovery_id).await.unwrap().unwrap();
        assert_eq!(recovery.config_path(), canonical_path);
        let push = read_frame(&mut stream).await.unwrap();
        assert_eq!(push["view"], adopted["result"]["outcome"]["view"]);
        handle.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn frame_error_type_is_used_by_real_socket_helpers() {
    let error = FrameError::FrameTooLarge;
    assert_eq!(error.to_string(), "frame-too-large");
}

#[tokio::test]
async fn provider_transfer_preview_and_export_are_target_scoped_and_secret_free_over_the_real_socket()
 {
    const SECRET: &str = "SOCKET_PROVIDER_IMPORT_SECRET_16001";
    let mut fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open-provider-transfer",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;

    let preview = request(
        &mut stream,
        "preview-provider-transfer",
        json!({
            "kind": "preview-provider-import",
            "target": "codex",
            "source": {
                "kind": "cc-switch",
                "payload": format!(
                    "ccswitch://v1/import?resource=provider&app=codex&name=Socket&endpoint=https%3A%2F%2Fsocket.example%2Fv1&apiKey={SECRET}&model=gpt-socket"
                )
            }
        }),
    )
    .await;
    assert_eq!(preview["type"], "response");
    assert_eq!(preview["result"]["kind"], "provider-import-preview");
    assert_eq!(
        preview["result"]["preview"]["candidates"][0]["credential"],
        "present"
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
        "confirm-provider-transfer",
        json!({
            "kind": "confirm-provider-import",
            "target": "codex",
            "previewToken": preview_token,
            "choices": [{
                "candidateId": candidate_id,
                "resolution": { "kind": "create" }
            }]
        }),
    )
    .await;
    assert_eq!(confirmed["type"], "response");
    assert_eq!(confirmed["result"]["kind"], "provider-import-outcome");
    assert_eq!(
        confirmed["result"]["outcome"]["records"][0]["resolution"],
        "created"
    );
    let provider_id = confirmed["result"]["outcome"]["records"][0]["providerId"]
        .as_str()
        .unwrap();
    let push = read_frame(&mut stream).await.unwrap();
    assert_eq!(push["type"], "target-view");
    assert_eq!(push["view"]["providers"][0]["id"], provider_id);
    assert_eq!(
        push["view"]["providers"][0]["importProvenance"]["sourceProduct"],
        "cc-switch"
    );
    assert_eq!(
        push["view"]["providers"][0]["importProvenance"]["sourceTarget"],
        "codex"
    );
    assert_eq!(push["view"]["providers"][0]["importedCurrent"], Value::Null);
    assert!(!confirmed.to_string().contains(SECRET));
    assert!(!push.to_string().contains(SECRET));
    let persisted = fixture.store.target_view_for(Target::Codex).await.unwrap();
    assert_eq!(persisted.providers[0].id.to_string(), provider_id);
    assert_eq!(persisted.current_provider_id, None);

    let export = request(
        &mut stream,
        "export-provider-transfer",
        json!({ "kind": "export-provider-configuration", "target": "codex" }),
    )
    .await;
    assert_eq!(export["type"], "response");
    assert_eq!(export["result"]["kind"], "provider-configuration-export");
    assert_eq!(
        export["result"]["export"]["targetProviders"][0]["credential"],
        "missing"
    );
    let export_text = export.to_string().to_ascii_lowercase();
    for forbidden in ["token", "recovery", "activatedsnapshot"] {
        assert!(!export_text.contains(forbidden));
    }

    let rejected = request(
        &mut stream,
        "reject-provider-transfer",
        json!({
            "kind": "preview-provider-import",
            "target": "codex",
            "source": {
                "kind": "cc-switch",
                "payload": format!(
                    "ccswitch://v1/import?resource=provider&app=codex&name=Socket&apiKey={SECRET}&apiKey=duplicate"
                )
            }
        }),
    )
    .await;
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["problem"]["code"], "duplicate-provider-import");
    assert!(!rejected.to_string().contains(SECRET));

    fixture.shutdown().await;
}

#[tokio::test]
async fn cc_switch_sql_provider_and_usage_migrate_clear_and_fail_closed_over_the_real_socket() {
    let mut fixture = ControlFixture::start().await;
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    request(
        &mut stream,
        "open-cc-switch-sql-import",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let export_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cc-switch-v3.19.2-export.sql");

    let preview = request(
        &mut stream,
        "preview-cc-switch-sql-import",
        json!({
            "kind": "preview-provider-import",
            "target": "codex",
            "source": {
                "kind": "cc-switch-sql",
                "path": export_path.to_string_lossy()
            }
        }),
    )
    .await;
    assert_eq!(preview["type"], "response");
    assert_eq!(
        preview["result"]["preview"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        preview["result"]["preview"]["historicalUsage"]["recordCount"],
        5
    );
    assert_eq!(
        preview["result"]["preview"]["historicalUsage"]["selectedByDefault"],
        false
    );
    let preview_text = preview.to_string();
    for forbidden in [
        "cc-switch-v3.19.2-export.sql",
        "ccswitch-codex-credential-fixture",
        "codex-session-secret-identity",
        "upstream-error-secret-payload",
    ] {
        assert!(!preview_text.contains(forbidden));
    }

    let preview_token = preview["result"]["preview"]["previewToken"]
        .as_str()
        .unwrap();
    let candidate_id = preview["result"]["preview"]["candidates"][0]["candidateId"]
        .as_str()
        .unwrap();
    let confirmed = request(
        &mut stream,
        "confirm-cc-switch-sql-import",
        json!({
            "kind": "confirm-provider-import",
            "target": "codex",
            "previewToken": preview_token,
            "choices": [{
                "candidateId": candidate_id,
                "resolution": { "kind": "create" }
            }],
            "includeHistoricalUsage": true
        }),
    )
    .await;
    assert_eq!(
        confirmed["result"]["outcome"]["historicalUsageImportedRecords"],
        5
    );
    let push = read_frame(&mut stream).await.unwrap();
    assert_eq!(
        push["view"]["providers"][0]["importProvenance"]["sourceProduct"],
        "cc-switch"
    );

    let activity = request(
        &mut stream,
        "list-migrated-usage",
        json!({
            "kind": "list-usage-activity",
            "target": "codex",
            "limit": 10
        }),
    )
    .await;
    let entries = activity["result"]["page"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry["kind"] == "migrated-usage-rollup")
    );
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry["rollup"]["sourceRecordCount"].as_u64().unwrap())
            .sum::<u64>(),
        5
    );
    assert!(
        entries
            .iter()
            .all(|entry| entry["rollup"]["sourceProduct"] == "cc-switch")
    );

    let duplicate = request(
        &mut stream,
        "preview-duplicate-cc-switch-usage",
        json!({
            "kind": "preview-provider-import",
            "target": "codex",
            "source": {
                "kind": "cc-switch-sql",
                "path": export_path.to_string_lossy()
            }
        }),
    )
    .await;
    let duplicate_error = request(
        &mut stream,
        "confirm-duplicate-cc-switch-usage",
        json!({
            "kind": "confirm-provider-import",
            "target": "codex",
            "previewToken": duplicate["result"]["preview"]["previewToken"],
            "choices": [],
            "includeHistoricalUsage": true
        }),
    )
    .await;
    assert_eq!(
        duplicate_error["problem"]["code"],
        "cc-switch-usage-already-imported"
    );

    let invalid = request(
        &mut stream,
        "reject-relative-cc-switch-export",
        json!({
            "kind": "preview-provider-import",
            "target": "codex",
            "source": { "kind": "cc-switch-sql", "path": "relative-export.sql" }
        }),
    )
    .await;
    assert_eq!(invalid["problem"]["code"], "invalid-cc-switch-export");
    assert!(!invalid.to_string().contains("relative-export.sql"));

    let cleared = request(
        &mut stream,
        "clear-migrated-usage",
        json!({ "kind": "clear-usage", "target": "codex" }),
    )
    .await;
    assert_eq!(
        cleared["result"]["outcome"]["clearedMigratedUsageRollups"],
        2
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn recovery_backup_create_and_inspect_are_private_global_and_secret_free_over_real_uds() {
    const PROVIDER_SECRET: &str = "RECOVERY_SOCKET_PROVIDER_SECRET_17011";
    const REFRESH_SECRET: &str = "RECOVERY_SOCKET_REFRESH_SECRET_17012";
    let mut fixture = ControlFixture::start().await;
    fixture
        .store
        .apply_provider_action(
            Uuid::new_v4(),
            0,
            json!({
                "kind": "create-provider",
                "name": "Recovery socket provider",
                "baseUrl": "https://recovery.example/v1",
                "model": "recovery-model",
                "credential": { "kind": "replace", "value": PROVIDER_SECRET },
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    let user_home = fixture.root.join("home");
    let account_path = user_home.join(".muxvia/state/subscription-accounts.json");
    fs::write(
        &account_path,
        format!(
            r#"{{"version":1,"accounts":{{"account":{{"account_id":"account","refresh_token":"{REFRESH_SECRET}","authenticated_at":1700000000,"state":"authorized"}}}},"default_account_id":"account"}}"#
        ),
    )
    .unwrap();
    fs::set_permissions(&account_path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir_all(user_home.join(".codex")).unwrap();
    fs::write(
        user_home.join(".codex/config.toml"),
        "model = \"recovery-socket\"\n",
    )
    .unwrap();
    fs::create_dir_all(user_home.join(".claude")).unwrap();
    fs::write(user_home.join(".claude/settings.json"), "{\"env\":{}}\n").unwrap();

    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let created = request(
        &mut stream,
        "create-recovery-backup",
        json!({ "kind": "create-recovery-backup" }),
    )
    .await;
    assert_eq!(created["type"], "response");
    assert_eq!(created["result"]["kind"], "recovery-backup-created");
    assert_eq!(created["result"]["inspection"]["sensitive"], true);
    assert_eq!(
        created["result"]["inspection"]["compatibility"],
        "compatible"
    );
    assert_eq!(
        created["result"]["inspection"]["entries"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let created_text = created.to_string();
    assert!(!created_text.contains(PROVIDER_SECRET));
    assert!(!created_text.contains(REFRESH_SECRET));
    let backup_path = PathBuf::from(created["result"]["path"].as_str().unwrap());
    assert_eq!(
        backup_path.parent(),
        Some(user_home.join(".muxvia/backups").as_path())
    );
    assert_eq!(
        fs::metadata(&backup_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let artifact = fs::read(&backup_path).unwrap();
    assert!(
        artifact
            .windows(PROVIDER_SECRET.len())
            .any(|window| window == PROVIDER_SECRET.as_bytes())
    );
    assert!(
        artifact
            .windows(REFRESH_SECRET.len())
            .any(|window| window == REFRESH_SECRET.as_bytes())
    );

    let inspected = request(
        &mut stream,
        "inspect-recovery-backup",
        json!({
            "kind": "inspect-recovery-backup",
            "path": backup_path.to_string_lossy()
        }),
    )
    .await;
    assert_eq!(inspected["result"]["kind"], "recovery-backup-inspection");
    assert_eq!(
        inspected["result"]["inspection"],
        created["result"]["inspection"]
    );
    assert!(!inspected.to_string().contains(PROVIDER_SECRET));
    assert!(!inspected.to_string().contains(REFRESH_SECRET));

    let rejected = request(
        &mut stream,
        "inspect-relative-recovery-backup",
        json!({ "kind": "inspect-recovery-backup", "path": "relative.backup" }),
    )
    .await;
    assert_eq!(rejected["problem"]["code"], "recovery-backup-invalid-path");
    assert!(!rejected.to_string().contains("relative.backup"));
    let opened = request(
        &mut stream,
        "open-after-recovery-backup",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    assert_eq!(opened["result"]["kind"], "target-view");
    fixture.shutdown().await;
}

#[tokio::test]
async fn recovery_backup_restore_replaces_the_installation_and_returns_a_safe_recovery_point() {
    const ORIGINAL_SECRET: &str = "RECOVERY_RESTORE_ORIGINAL_SECRET_18011";
    const REPLACEMENT_SECRET: &str = "RECOVERY_RESTORE_REPLACEMENT_SECRET_18012";
    let mut fixture = ControlFixture::start().await;
    fixture
        .store
        .apply_provider_action(
            Uuid::new_v4(),
            0,
            json!({
                "kind": "create-provider",
                "name": "Original recovery provider",
                "baseUrl": "https://original-recovery.example/v1",
                "model": "original-recovery-model",
                "credential": { "kind": "replace", "value": ORIGINAL_SECRET },
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    let user_home = fixture.root.join("home");
    fs::create_dir_all(user_home.join(".codex")).unwrap();
    fs::write(
        user_home.join(".codex/config.toml"),
        "model = \"original-recovery-model\"\n",
    )
    .unwrap();

    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let created = request(
        &mut stream,
        "create-selected-recovery-backup",
        json!({ "kind": "create-recovery-backup" }),
    )
    .await;
    let selected_path = created["result"]["path"].as_str().unwrap().to_owned();

    fixture
        .store
        .apply_provider_action(
            Uuid::new_v4(),
            1,
            json!({
                "kind": "create-provider",
                "name": "Replacement recovery provider",
                "baseUrl": "https://replacement-recovery.example/v1",
                "model": "replacement-recovery-model",
                "credential": { "kind": "replace", "value": REPLACEMENT_SECRET },
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    fs::write(
        user_home.join(".codex/config.toml"),
        "model = \"replacement-recovery-model\"\n",
    )
    .unwrap();

    let restored = request(
        &mut stream,
        "restore-selected-recovery-backup",
        json!({
            "kind": "restore-recovery-backup",
            "path": selected_path,
            "acknowledgement": "replace-current-installation",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "unset",
                "hostManagedState": "unmanaged",
                "cwd": user_home
            }
        }),
    )
    .await;
    assert_eq!(restored["type"], "response", "restore frame: {restored:?}");
    assert_eq!(restored["result"]["kind"], "recovery-backup-restored");
    assert_eq!(restored["result"]["sensitive"], true);
    assert_eq!(restored["result"]["restartTargetClis"], true);
    let recovery_path = PathBuf::from(restored["result"]["preRestoreBackupPath"].as_str().unwrap());
    assert!(recovery_path.exists());
    let restored_text = restored.to_string();
    assert!(!restored_text.contains(ORIGINAL_SECRET));
    assert!(!restored_text.contains(REPLACEMENT_SECRET));
    assert!(
        fs::read(&recovery_path)
            .unwrap()
            .windows(REPLACEMENT_SECRET.len())
            .any(|window| window == REPLACEMENT_SECRET.as_bytes())
    );

    let opened = request(
        &mut stream,
        "open-restored-target",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let providers = opened["result"]["view"]["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["name"], "Original recovery provider");
    assert_eq!(
        fs::read_to_string(user_home.join(".codex/config.toml")).unwrap(),
        "model = \"original-recovery-model\"\n"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn recovery_backup_restore_restarts_the_restored_takeover_before_success() {
    let mut fixture = ControlFixture::start().await;
    let user_home = fixture.root.join("home");
    let mut stream = fixture.connect().await;
    hello(&mut stream).await;
    let _ = request(
        &mut stream,
        "open-takeover-recovery-target",
        json!({ "kind": "open-target", "target": "codex" }),
    )
    .await;
    let saved = request(
        &mut stream,
        "save-original-takeover-provider",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 0,
            "action": {
                "kind": "create-provider", "name": "Original takeover provider",
                "baseUrl": "https://original-takeover.example/v1", "model": "original-model",
                "credential": { "kind": "replace", "value": "ORIGINAL_TAKEOVER_SECRET_18031" },
                "presetKey": null
            }
        }),
    )
    .await;
    let original_provider_id = saved["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _ = read_frame(&mut stream).await.unwrap();
    let activated = request(
        &mut stream,
        "activate-original-takeover-provider",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 1,
            "action": {
                "kind": "activate-provider", "providerId": original_provider_id,
                "mode": "takeover"
            }
        }),
    )
    .await;
    assert_eq!(activated["result"]["outcome"]["status"], "applied");
    let _ = read_frame(&mut stream).await.unwrap();
    let original_port = fixture
        .store
        .committed_takeover_for(Target::Codex)
        .await
        .unwrap()
        .unwrap()
        .route_port;

    let created = request(
        &mut stream,
        "create-takeover-recovery-backup",
        json!({ "kind": "create-recovery-backup" }),
    )
    .await;
    let selected_path = created["result"]["path"].as_str().unwrap();

    let replacement = request(
        &mut stream,
        "save-replacement-takeover-provider",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 2,
            "action": {
                "kind": "create-provider", "name": "Replacement takeover provider",
                "baseUrl": "https://replacement-takeover.example/v1", "model": "replacement-model",
                "credential": { "kind": "replace", "value": "REPLACEMENT_TAKEOVER_SECRET_18032" },
                "presetKey": null
            }
        }),
    )
    .await;
    let replacement_provider_id = replacement["result"]["outcome"]["view"]["providers"][1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let _ = read_frame(&mut stream).await.unwrap();
    let replacement = request(
        &mut stream,
        "activate-replacement-takeover-provider",
        json!({
            "kind": "act", "target": "codex", "actionId": Uuid::new_v4(),
            "expectedRevision": 3,
            "action": {
                "kind": "activate-provider", "providerId": replacement_provider_id,
                "mode": "takeover"
            }
        }),
    )
    .await;
    assert_eq!(replacement["result"]["outcome"]["status"], "applied");
    let _ = read_frame(&mut stream).await.unwrap();

    let restored = request(
        &mut stream,
        "restore-takeover-recovery-backup",
        json!({
            "kind": "restore-recovery-backup",
            "path": selected_path,
            "acknowledgement": "replace-current-installation",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "unset",
                "hostManagedState": "unmanaged",
                "cwd": user_home
            }
        }),
    )
    .await;
    assert_eq!(restored["type"], "response", "restore frame: {restored:?}");
    assert_eq!(restored["result"]["resumedTakeovers"], json!(["codex"]));
    let committed = fixture
        .store
        .committed_takeover_for(Target::Codex)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.route_port, original_port);
    tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, original_port))
        .await
        .expect("restored takeover is accepting connections");
    assert!(!restored.to_string().contains("TAKEOVER_SECRET"));
    fixture.shutdown().await;
}
