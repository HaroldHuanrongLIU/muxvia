use std::{
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode},
    response::Response,
    routing::post,
};
use futures_util::{Stream, StreamExt, stream};
use muxvia_routing::{
    home::MuxviaHome,
    model::{
        ModelServer, ModelServerStatus, ReqwestUpstream, ReservedListener, UpstreamError,
        UpstreamRequest, UpstreamResponse, UpstreamTransport,
        auth::{ROUTING_CREDENTIAL_LEN, routing_credential_matches},
        headers::{forward_request_headers, forward_response_headers},
    },
    state::StateStore,
};
use secrecy::SecretString;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, oneshot},
    time::{sleep, timeout},
};
use uuid::Uuid;

const ROUTING_CREDENTIAL: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PROVIDER_SECRET: &str = "provider-secret-must-not-escape";

#[test]
fn request_header_policy_removes_static_dynamic_and_local_credentials() {
    let mut incoming = HeaderMap::new();
    incoming.append(
        "connection",
        HeaderValue::from_static("keep-alive, X-Remove-Me"),
    );
    incoming.insert("x-remove-me", HeaderValue::from_static("secret-hop"));
    incoming.insert("keep-alive", HeaderValue::from_static("timeout=5"));
    incoming.insert("proxy-connection", HeaderValue::from_static("close"));
    incoming.insert("proxy-private", HeaderValue::from_static("never-forward"));
    incoming.insert("te", HeaderValue::from_static("trailers"));
    incoming.insert("trailer", HeaderValue::from_static("x-checksum"));
    incoming.insert("upgrade", HeaderValue::from_static("websocket"));
    incoming.insert("host", HeaderValue::from_static("127.0.0.1"));
    incoming.insert("content-length", HeaderValue::from_static("123"));
    incoming.insert("authorization", HeaderValue::from_static("Bearer incoming"));
    incoming.insert(
        "x-muxvia-routing-credential",
        HeaderValue::from_static("local-secret"),
    );
    incoming.insert("openai-beta", HeaderValue::from_static("responses=v1"));
    incoming.insert("x-correlation-id", HeaderValue::from_static("abc"));

    let forwarded = forward_request_headers(&incoming, &SecretString::from(PROVIDER_SECRET))
        .expect("provider credential is valid HTTP metadata");

    assert_eq!(forwarded.len(), 3);
    assert_eq!(
        forwarded["authorization"],
        "Bearer provider-secret-must-not-escape"
    );
    assert_eq!(forwarded["openai-beta"], "responses=v1");
    assert_eq!(forwarded["x-correlation-id"], "abc");
}

#[test]
fn response_header_policy_removes_connection_nominated_and_framing_headers() {
    let mut incoming = HeaderMap::new();
    incoming.append(
        "connection",
        HeaderValue::from_static("X-Private, keep-alive"),
    );
    incoming.append("connection", HeaderValue::from_static("X-Also-Private"));
    incoming.insert("x-private", HeaderValue::from_static("one"));
    incoming.insert("x-also-private", HeaderValue::from_static("two"));
    incoming.insert("keep-alive", HeaderValue::from_static("timeout=5"));
    incoming.insert("transfer-encoding", HeaderValue::from_static("chunked"));
    incoming.insert("content-length", HeaderValue::from_static("999"));
    incoming.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    incoming.insert("x-request-id", HeaderValue::from_static("upstream-id"));

    let forwarded = forward_response_headers(&incoming);

    assert_eq!(forwarded.len(), 2);
    assert_eq!(forwarded["content-type"], "text/event-stream");
    assert_eq!(forwarded["x-request-id"], "upstream-id");
}

#[test]
fn routing_credential_comparison_rejects_all_malformed_shapes() {
    assert_eq!(ROUTING_CREDENTIAL.len(), ROUTING_CREDENTIAL_LEN);
    let expected = SecretString::from(ROUTING_CREDENTIAL);

    let candidates: Vec<Vec<HeaderValue>> = vec![
        vec![],
        vec![HeaderValue::from_static("wrong")],
        vec![HeaderValue::from_str(&"x".repeat(ROUTING_CREDENTIAL_LEN - 1)).unwrap()],
        vec![HeaderValue::from_str(&"x".repeat(ROUTING_CREDENTIAL_LEN + 1)).unwrap()],
        vec![HeaderValue::from_bytes(&[0xff; ROUTING_CREDENTIAL_LEN]).unwrap()],
        vec![
            HeaderValue::from_static(ROUTING_CREDENTIAL),
            HeaderValue::from_static(ROUTING_CREDENTIAL),
        ],
    ];

    for values in candidates {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append("x-muxvia-routing-credential", value);
        }
        assert!(!routing_credential_matches(&headers, &expected));
    }

    let mut valid = HeaderMap::new();
    valid.insert(
        "x-muxvia-routing-credential",
        HeaderValue::from_static(ROUTING_CREDENTIAL),
    );
    assert!(routing_credential_matches(&valid, &expected));
}

struct StoreFixture {
    _home: TempDir,
    muxvia_home: MuxviaHome,
    store: Arc<StateStore>,
}

struct StoredRequestRecord {
    outcome: String,
    http_status: Option<u16>,
    provider_id: Option<String>,
    provider_name: Option<String>,
    model: String,
    error_payload: Option<Vec<u8>>,
    error_payload_truncated: bool,
    usage: Option<(u64, u64, u64, u64)>,
}

impl StoreFixture {
    async fn new() -> Self {
        let home = TempDir::new().unwrap();
        let muxvia_home = MuxviaHome::from_user_home(home.path());
        let store = Arc::new(StateStore::open(&muxvia_home).await.unwrap());
        Self {
            _home: home,
            muxvia_home,
            store,
        }
    }

    async fn set_routing_credential(&self) {
        let database = tokio_rusqlite::Connection::open(self.muxvia_home.database_path())
            .await
            .unwrap();
        database
            .call(|connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "UPDATE target_route_state SET routing_credential = ?1 WHERE target = 'codex'",
                    [ROUTING_CREDENTIAL],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn seed_snapshot(&self, upstream_base_url: &str) -> (Uuid, Uuid) {
        self.seed_snapshot_for_model(upstream_base_url, "gpt-test")
            .await
    }

    async fn seed_snapshot_for_model(&self, upstream_base_url: &str, model: &str) -> (Uuid, Uuid) {
        self.set_routing_credential().await;
        let snapshot_id = Uuid::new_v4();
        let provider_id = Uuid::new_v4();
        let plan_id = Uuid::new_v4();
        let plan_epoch = Uuid::new_v4();
        let database = tokio_rusqlite::Connection::open(self.muxvia_home.database_path())
            .await
            .unwrap();
        let upstream_base_url = upstream_base_url.to_owned();
        let model = model.to_owned();
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                let transaction = connection.transaction()?;
                transaction.execute(
                    "INSERT INTO credentials (id, target, bearer_token)
                     VALUES (?1, 'codex', ?2)",
                    (provider_id.to_string(), PROVIDER_SECRET),
                )?;
                transaction.execute(
                    "INSERT INTO providers
                     (id, target, position, provider_revision, name, base_url, model, protocol,
                      authentication, routing_requirement, credential_id, provenance_kind,
                      provenance_key, generated_owner_id)
                     VALUES (?1, 'codex', 0, 1, 'Fake upstream', ?2, ?3,
                             'openai-responses', 'openai-bearer', 'direct-compatible',
                             ?1, NULL, NULL, NULL)",
                    (
                        provider_id.to_string(),
                        upstream_base_url.clone(),
                        model.clone(),
                    ),
                )?;
                transaction.execute(
                    "INSERT INTO activated_snapshots
                     (id, target, provider_id, base_url, model, protocol, authentication,
                      provider_bearer_token, epoch)
                     VALUES (?1, 'codex', ?2, ?3, ?4, 'openai-responses',
                             'openai-bearer', ?5, ?6)",
                    (
                        snapshot_id.to_string(),
                        provider_id.to_string(),
                        upstream_base_url.clone(),
                        model.clone(),
                        PROVIDER_SECRET,
                        Uuid::new_v4().to_string(),
                    ),
                )?;
                transaction.execute("DELETE FROM failover_draft_members WHERE target = 'codex'", [])?;
                transaction.execute(
                    "INSERT INTO failover_draft_members (target, position, provider_id, provider_revision)
                     VALUES ('codex', 0, ?1, 1)",
                    [provider_id.to_string()],
                )?;
                transaction.execute(
                    "INSERT INTO activated_route_plans (id, target, epoch, created_revision)
                     VALUES (?1, 'codex', ?2, 0)",
                    (plan_id.to_string(), plan_epoch.to_string()),
                )?;
                transaction.execute(
                    "INSERT INTO activated_route_plan_members
                     (plan_id, position, provider_id, provider_revision, name, base_url, model,
                      protocol, authentication, credential_id, routing_requirement)
                     VALUES (?1, 0, ?2, 1, 'Fake upstream', ?3, ?4,
                             'openai-responses', 'openai-bearer', ?5, 'direct-compatible')",
                    (
                        plan_id.to_string(),
                        provider_id.to_string(),
                        upstream_base_url,
                        model,
                        provider_id.to_string(),
                    ),
                )?;
                transaction.execute(
                    "UPDATE target_route_state
                     SET activated_snapshot_id = ?1, current_provider_id = ?2,
                         active_route_plan_id = ?3
                     WHERE target = 'codex'",
                    (
                        snapshot_id.to_string(),
                        provider_id.to_string(),
                        plan_id.to_string(),
                    ),
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        (snapshot_id, provider_id)
    }

    async fn drop_secret_table(&self) {
        let database = tokio_rusqlite::Connection::open(self.muxvia_home.database_path())
            .await
            .unwrap();
        database
            .call(|connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute_batch("PRAGMA foreign_keys = OFF")?;
                connection.execute("DELETE FROM providers", [])?;
                connection.execute("DELETE FROM credentials", [])?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn request_records(&self) -> Vec<StoredRequestRecord> {
        let database = tokio_rusqlite::Connection::open(self.muxvia_home.database_path())
            .await
            .unwrap();
        database
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT outcome, http_status, provider_id, provider_name, model,
                            error_payload, error_payload_truncated, usage_observed,
                            input_tokens, cached_input_tokens, cache_creation_input_tokens,
                            output_tokens
                     FROM request_records ORDER BY sequence",
                )?;
                statement
                    .query_map([], |row| {
                        let usage_observed = row.get::<_, bool>(7)?;
                        Ok(StoredRequestRecord {
                            outcome: row.get(0)?,
                            http_status: row.get(1)?,
                            provider_id: row.get(2)?,
                            provider_name: row.get(3)?,
                            model: row.get(4)?,
                            error_payload: row.get(5)?,
                            error_payload_truncated: row.get(6)?,
                            usage: usage_observed
                                .then(|| {
                                    Ok::<_, tokio_rusqlite::rusqlite::Error>((
                                        row.get(8)?,
                                        row.get(9)?,
                                        row.get(10)?,
                                        row.get(11)?,
                                    ))
                                })
                                .transpose()?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .unwrap()
    }
}

async fn start_model(fixture: &StoreFixture) -> muxvia_routing::model::ModelServerHandle {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let reserved = ReservedListener::new(listener).unwrap();
    ModelServer::bind_reserved(
        reserved,
        Arc::clone(&fixture.store),
        Arc::new(ReqwestUpstream::new().unwrap()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn model_handle_reports_readiness_and_durable_unexpected_task_exit() {
    let fixture = StoreFixture::new().await;
    let mut handle = start_model(&fixture).await;
    assert_eq!(handle.status(), ModelServerStatus::Running);
    assert!(handle.is_running());

    handle.abort();
    tokio::task::yield_now().await;

    assert_eq!(handle.status(), ModelServerStatus::Failed);
    assert!(!handle.is_running());
    assert!(handle.shutdown().await.is_err());
}

fn route_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap()
}

async fn post_route(endpoint: SocketAddr, credential: Option<HeaderValue>) -> reqwest::Response {
    let mut request = route_client()
        .post(format!("http://{endpoint}/v1/responses"))
        .body("{\"input\":\"hello\"}");
    if let Some(credential) = credential {
        request = request.header("x-muxvia-routing-credential", credential);
    }
    request.send().await.unwrap()
}

async fn post_route_headers(endpoint: SocketAddr, headers: HeaderMap) -> reqwest::Response {
    route_client()
        .post(format!("http://{endpoint}/v1/responses"))
        .headers(headers)
        .body("{\"input\":\"hello\"}")
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn reserved_listener_requires_exact_ipv4_localhost_and_model_exposes_only_responses_post() {
    let fixture = StoreFixture::new().await;
    let any = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
    assert!(ReservedListener::new(any).is_err());
    let ipv6 = TcpListener::bind(("::", 0)).await.unwrap();
    assert!(ReservedListener::new(ipv6).is_err());

    let server = start_model(&fixture).await;
    assert_eq!(server.endpoint().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(
        route_client()
            .get(format!("http://{}/v1/responses", server.endpoint()))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        route_client()
            .post(format!("http://{}/v1/other", server.endpoint()))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn every_invalid_credential_shape_returns_the_same_401_before_state_or_upstream_access() {
    let upstream = FakeUpstream::start(StatusCode::OK).await;
    let fixture = StoreFixture::new().await;
    fixture.seed_snapshot(&upstream.base_url()).await;
    fixture.drop_secret_table().await;
    let server = start_model(&fixture).await;

    let missing = post_route(server.endpoint(), None).await;
    let wrong = post_route(server.endpoint(), Some(HeaderValue::from_static("wrong"))).await;
    let short = post_route(
        server.endpoint(),
        Some(HeaderValue::from_str(&"x".repeat(63)).unwrap()),
    )
    .await;
    let long = post_route(
        server.endpoint(),
        Some(HeaderValue::from_str(&"x".repeat(65)).unwrap()),
    )
    .await;
    let mut duplicate_headers = HeaderMap::new();
    duplicate_headers.append(
        "x-muxvia-routing-credential",
        HeaderValue::from_static(ROUTING_CREDENTIAL),
    );
    duplicate_headers.append(
        "x-muxvia-routing-credential",
        HeaderValue::from_static(ROUTING_CREDENTIAL),
    );
    let duplicate = post_route_headers(server.endpoint(), duplicate_headers).await;
    let mut non_ascii_headers = HeaderMap::new();
    non_ascii_headers.insert(
        "x-muxvia-routing-credential",
        HeaderValue::from_bytes(&[0xff; ROUTING_CREDENTIAL_LEN]).unwrap(),
    );
    let non_ascii = post_route_headers(server.endpoint(), non_ascii_headers).await;

    let mut observed = Vec::new();
    for response in [missing, wrong, short, long, duplicate, non_ascii] {
        let status = response.status();
        let mut headers = response.headers().clone();
        headers.remove("date");
        let body = response.bytes().await.unwrap();
        observed.push((status, headers, body));
    }
    assert!(observed.iter().all(|item| item == &observed[0]));
    assert_eq!(observed[0].0, StatusCode::UNAUTHORIZED);
    assert!(!String::from_utf8_lossy(&observed[0].2).contains("credential"));
    assert_eq!(upstream.state.calls.load(Ordering::SeqCst), 0);
    server.shutdown().await.unwrap();
    upstream.shutdown().await;
}

#[tokio::test]
async fn absent_snapshot_returns_503_without_reading_provider_credentials() {
    let fixture = StoreFixture::new().await;
    fixture.set_routing_credential().await;
    fixture.drop_secret_table().await;
    let server = start_model(&fixture).await;

    let response = post_route(
        server.endpoint(),
        Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
    )
    .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn upstream_connect_failure_returns_502_before_response_commitment() {
    let fixture = StoreFixture::new().await;
    let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    fixture
        .seed_snapshot(&format!("http://127.0.0.1:{port}/api/v1/"))
        .await;
    let server = start_model(&fixture).await;

    let response = post_route(
        server.endpoint(),
        Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_late_success_records_the_provider_from_its_pinned_snapshot() {
    let fixture = StoreFixture::new().await;
    let (first_snapshot, first_provider) =
        fixture.seed_snapshot("https://first.example/api/v1/").await;
    let (_second_snapshot, second_provider) = fixture
        .seed_snapshot("https://second.example/api/v1/")
        .await;
    let before = fixture.store.target_view().await.unwrap();

    let recorded = fixture.store.record_serving(first_snapshot).await.unwrap();

    assert_eq!(
        recorded.current_provider_id,
        Some(second_provider.to_string())
    );
    assert_eq!(
        recorded.activated_snapshot.unwrap().provider_id,
        second_provider
    );
    assert_eq!(
        recorded.serving_provider_id,
        Some(first_provider.to_string())
    );
    assert_eq!(recorded.management_revision, before.management_revision);
    assert_eq!(recorded.view_sequence, before.view_sequence + 1);
}

#[derive(Clone)]
struct FakeState {
    calls: Arc<AtomicUsize>,
    capture: SharedCapture,
    status: StatusCode,
    body_dropped: Arc<AtomicBool>,
    first_body_chunk_gate: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    error_body: Arc<Vec<u8>>,
}

type SharedCapture = Arc<Mutex<Option<(String, HeaderMap, Vec<u8>)>>>;
type ByteChunkStream = Pin<Box<dyn Stream<Item = Result<&'static [u8], Infallible>> + Send>>;

struct ObservedStream {
    chunks: ByteChunkStream,
    dropped: Arc<AtomicBool>,
}

impl Stream for ObservedStream {
    type Item = Result<&'static [u8], Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.chunks.as_mut().poll_next(context)
    }
}

impl Drop for ObservedStream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

async fn fake_responses(State(state): State<FakeState>, request: Request<Body>) -> Response<Body> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    let path = request.uri().path().to_owned();
    let headers = request.headers().clone();
    let mut body = request.into_body().into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    *state.capture.lock().await = Some((path, headers, bytes));

    if state.status == StatusCode::TOO_MANY_REQUESTS {
        return Response::builder()
            .status(state.status)
            .header("content-type", "application/json")
            .header("x-upstream-error", "rate-limited")
            .body(Body::from(state.error_body.as_ref().clone()))
            .unwrap();
    }

    let first_body_chunk_gate = state.first_body_chunk_gate.lock().await.take();
    let chunks = stream::unfold(
        (0, first_body_chunk_gate),
        |(index, mut first_body_chunk_gate)| async move {
            if index == 0
                && let Some(gate) = first_body_chunk_gate.take()
            {
                let _ = gate.await;
            }
            let chunk: &'static [u8] = match index {
                0 => b"data: {\"type\":\"response.created\"}\n\n",
                1 => b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                2 => b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":20},\"output_tokens\":30}}}\n\n",
                3 => b"data: [DONE]\n\n",
                _ => return None,
            };
            sleep(if index == 0 {
                Duration::from_millis(25)
            } else {
                Duration::from_millis(150)
            })
            .await;
            Some((Ok(chunk), (index + 1, first_body_chunk_gate)))
        },
    );
    Response::builder()
        .status(state.status)
        .header("content-type", "text/event-stream")
        .header("x-upstream", "kept")
        .header("connection", "x-remove-response")
        .header("x-remove-response", "secret-hop")
        .body(Body::from_stream(ObservedStream {
            chunks: Box::pin(chunks),
            dropped: Arc::clone(&state.body_dropped),
        }))
        .unwrap()
}

struct FakeUpstream {
    endpoint: SocketAddr,
    state: FakeState,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct StreamingRequestState {
    first_chunk: Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>,
}

async fn observe_streaming_request(
    State(state): State<StreamingRequestState>,
    request: Request<Body>,
) -> Response<Body> {
    let mut body = request.into_body().into_data_stream();
    let first = body.next().await.unwrap().unwrap().to_vec();
    if let Some(sender) = state.first_chunk.lock().await.take() {
        let _ = sender.send(first);
    }
    while let Some(chunk) = body.next().await {
        chunk.unwrap();
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap()
}

impl FakeUpstream {
    async fn start(status: StatusCode) -> Self {
        Self::start_with_body_gate(status, None).await
    }

    async fn start_with_error_body(error_body: Vec<u8>) -> Self {
        Self::start_with_body_gate_and_error(StatusCode::TOO_MANY_REQUESTS, None, error_body).await
    }

    async fn start_with_blocked_body(status: StatusCode) -> (Self, oneshot::Sender<()>) {
        let (release, gate) = oneshot::channel();
        (
            Self::start_with_body_gate(status, Some(gate)).await,
            release,
        )
    }

    async fn start_with_body_gate(
        status: StatusCode,
        first_body_chunk_gate: Option<oneshot::Receiver<()>>,
    ) -> Self {
        Self::start_with_body_gate_and_error(
            status,
            first_body_chunk_gate,
            b"{\"error\":\"slow down\"}".to_vec(),
        )
        .await
    }

    async fn start_with_body_gate_and_error(
        status: StatusCode,
        first_body_chunk_gate: Option<oneshot::Receiver<()>>,
        error_body: Vec<u8>,
    ) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let state = FakeState {
            calls: Arc::new(AtomicUsize::new(0)),
            capture: Arc::new(Mutex::new(None)),
            status,
            body_dropped: Arc::new(AtomicBool::new(false)),
            first_body_chunk_gate: Arc::new(Mutex::new(first_body_chunk_gate)),
            error_body: Arc::new(error_body),
        };
        let router = Router::new()
            .route("/api/v1/responses", post(fake_responses))
            .with_state(state.clone());
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        Self {
            endpoint,
            state,
            shutdown: Some(shutdown),
            task,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/api/v1/", self.endpoint)
    }

    async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.await.unwrap();
    }
}

#[tokio::test]
async fn request_body_is_bounded_and_replayable_before_the_first_upstream_attempt() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let upstream_endpoint = listener.local_addr().unwrap();
    let (first_tx, mut first_rx) = oneshot::channel();
    let router = Router::new()
        .route("/api/v1/responses", post(observe_streaming_request))
        .with_state(StreamingRequestState {
            first_chunk: Arc::new(Mutex::new(Some(first_tx))),
        });
    let upstream_task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let fixture = StoreFixture::new().await;
    fixture
        .seed_snapshot(&format!("http://{upstream_endpoint}/api/v1/"))
        .await;
    let server = start_model(&fixture).await;
    let (finish_tx, finish_rx) = oneshot::channel::<()>();
    let request_stream = stream::unfold((0_u8, Some(finish_rx)), |(step, finish)| async move {
        match step {
            0 => Some((
                Ok::<_, Infallible>(b"first-request-chunk".as_slice()),
                (1, finish),
            )),
            1 => {
                let _ = finish.unwrap().await;
                Some((
                    Ok::<_, Infallible>(b"last-request-chunk".as_slice()),
                    (2, None),
                ))
            }
            _ => None,
        }
    });
    let endpoint = server.endpoint();
    let request_task = tokio::spawn(async move {
        route_client()
            .post(format!("http://{endpoint}/v1/responses"))
            .header("x-muxvia-routing-credential", ROUTING_CREDENTIAL)
            .body(reqwest::Body::wrap_stream(request_stream))
            .send()
            .await
            .unwrap()
    });

    assert!(
        timeout(Duration::from_millis(250), &mut first_rx)
            .await
            .is_err(),
        "the first upstream attempt began before the replayable request body reached EOF"
    );
    finish_tx.send(()).unwrap();
    let first = timeout(Duration::from_secs(1), first_rx)
        .await
        .expect("the bounded request was not forwarded after EOF")
        .unwrap();
    assert_eq!(first, b"first-request-chunklast-request-chunk");
    assert_eq!(request_task.await.unwrap().status(), StatusCode::OK);

    server.shutdown().await.unwrap();
    upstream_task.abort();
    let _ = upstream_task.await;
}

#[tokio::test]
async fn upstream_429_status_headers_and_body_pass_through_without_serving_update() {
    let upstream = FakeUpstream::start(StatusCode::TOO_MANY_REQUESTS).await;
    let fixture = StoreFixture::new().await;
    fixture.seed_snapshot(&upstream.base_url()).await;
    let before = fixture.store.target_view().await.unwrap();
    let server = start_model(&fixture).await;

    let response = post_route(
        server.endpoint(),
        Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["x-upstream-error"], "rate-limited");
    assert_eq!(response.text().await.unwrap(), "{\"error\":\"slow down\"}");
    let after = fixture.store.target_view().await.unwrap();
    assert_eq!(after.serving_provider_id, None);
    assert_eq!(after.view_sequence, before.view_sequence + 1);

    server.shutdown().await.unwrap();
    upstream.shutdown().await;
}

#[tokio::test]
async fn final_upstream_error_payload_is_bounded_redacted_and_marked_truncated() {
    let error_body = format!(
        "provider={PROVIDER_SECRET};routing={ROUTING_CREDENTIAL};{}",
        "x".repeat(70_000)
    )
    .into_bytes();
    let upstream = FakeUpstream::start_with_error_body(error_body.clone()).await;
    let fixture = StoreFixture::new().await;
    let (_snapshot_id, provider_id) = fixture.seed_snapshot(&upstream.base_url()).await;
    let server = start_model(&fixture).await;

    let response = post_route(
        server.endpoint(),
        Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
    )
    .await;
    let status = response.status();
    let forwarded = response.bytes().await.unwrap();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        forwarded.as_ref() == error_body.as_slice(),
        "upstream error payload changed in transit"
    );
    server.shutdown().await.unwrap();

    let records = fixture.request_records().await;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    let stored = record.error_payload.as_deref().unwrap_or_default();
    assert!(
        !stored
            .windows(PROVIDER_SECRET.len())
            .any(|window| window == PROVIDER_SECRET.as_bytes()),
        "stored failure payload contains a provider credential"
    );
    assert!(
        !stored
            .windows(ROUTING_CREDENTIAL.len())
            .any(|window| window == ROUTING_CREDENTIAL.as_bytes()),
        "stored failure payload contains a routing credential"
    );
    assert_eq!(record.outcome, "upstream-error");
    assert_eq!(record.http_status, Some(429));
    assert_eq!(
        record.provider_id.as_deref(),
        Some(provider_id.to_string().as_str())
    );
    assert_eq!(record.provider_name.as_deref(), Some("Fake upstream"));
    assert_eq!(record.model, "gpt-test");
    assert_eq!(stored.len(), 65_536);
    assert!(record.error_payload_truncated);

    upstream.shutdown().await;
}

#[tokio::test]
async fn exhausted_transport_failure_persists_the_final_attempt_without_a_payload() {
    let fixture = StoreFixture::new().await;
    let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let (_snapshot_id, provider_id) = fixture
        .seed_snapshot(&format!("http://127.0.0.1:{port}/api/v1/"))
        .await;
    let server = start_model(&fixture).await;

    let response = post_route(
        server.endpoint(),
        Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    server.shutdown().await.unwrap();

    let records = fixture.request_records().await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, "transport-error");
    assert_eq!(
        records[0].provider_id.as_deref(),
        Some(provider_id.to_string().as_str())
    );
    assert_eq!(records[0].provider_name.as_deref(), Some("Fake upstream"));
    assert_eq!(records[0].model, "gpt-test");
    assert_eq!(records[0].http_status, None);
    assert!(records[0].error_payload.is_none());
    assert!(!records[0].error_payload_truncated);
}

#[tokio::test]
async fn successful_route_appends_path_forwards_bytes_and_streams_sse_in_order() {
    let (upstream, release_upstream_body) =
        FakeUpstream::start_with_blocked_body(StatusCode::OK).await;
    let fixture = StoreFixture::new().await;
    let (_snapshot_id, provider_id) = fixture.seed_snapshot(&upstream.base_url()).await;
    let before = fixture.store.target_view().await.unwrap();
    let mut updates = fixture.store.subscribe_target_views();
    let server = start_model(&fixture).await;

    let response = timeout(
        Duration::from_secs(1),
        route_client()
            .post(format!("http://{}/v1/responses", server.endpoint()))
            .header("x-muxvia-routing-credential", ROUTING_CREDENTIAL)
            .header("authorization", "Bearer incoming")
            .header("connection", "x-remove-me")
            .header("x-remove-me", "secret-hop")
            .header("openai-beta", "responses=v1")
            .header("x-correlation-id", "abc")
            .body("request-body-streamed")
            .send(),
    )
    .await
    .expect("response head must not wait for the full upstream SSE body")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["x-upstream"], "kept");
    assert!(!response.headers().contains_key("x-remove-response"));

    let pushed = timeout(Duration::from_secs(1), updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pushed.serving_provider_id, Some(provider_id.to_string()));
    assert_eq!(pushed.management_revision, before.management_revision);
    assert_eq!(pushed.view_sequence, before.view_sequence + 1);
    assert_eq!(pushed.current_provider_id, before.current_provider_id);
    assert_eq!(
        pushed.activated_snapshot.as_ref().unwrap().provider_id,
        provider_id
    );

    let mut stream = response.bytes_stream();
    release_upstream_body.send(()).unwrap();
    let first = timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("first SSE chunk should arrive before the whole response")
        .unwrap()
        .unwrap();
    assert_eq!(first, "data: {\"type\":\"response.created\"}\n\n");
    let mut all = first.to_vec();
    while let Some(chunk) = stream.next().await {
        all.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(
        String::from_utf8(all).unwrap(),
        concat!(
            "data: {\"type\":\"response.created\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":20},\"output_tokens\":30}}}\n\n",
            "data: [DONE]\n\n"
        )
    );

    let capture = upstream.state.capture.lock().await.clone().unwrap();
    assert_eq!(capture.0, "/api/v1/responses");
    assert_eq!(capture.2, b"request-body-streamed");
    assert_eq!(
        capture.1["authorization"],
        "Bearer provider-secret-must-not-escape"
    );
    assert_eq!(capture.1["openai-beta"], "responses=v1");
    assert_eq!(capture.1["x-correlation-id"], "abc");
    assert!(!capture.1.contains_key("x-muxvia-routing-credential"));
    assert!(!capture.1.contains_key("x-remove-me"));

    let repeated = post_route(
        server.endpoint(),
        Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
    )
    .await;
    assert_eq!(repeated.status(), StatusCode::OK);
    let repeated_view = timeout(Duration::from_secs(1), updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated_view.view_sequence, before.view_sequence + 2);
    assert_eq!(
        repeated_view.management_revision,
        before.management_revision
    );
    assert_eq!(
        repeated_view.serving_provider_id,
        Some(provider_id.to_string())
    );
    repeated.bytes().await.unwrap();

    server.shutdown().await.unwrap();
    upstream.shutdown().await;
}

#[tokio::test]
async fn successful_codex_stream_persists_one_priced_request_record_after_body_completion() {
    let upstream = FakeUpstream::start(StatusCode::OK).await;
    let fixture = StoreFixture::new().await;
    let (_snapshot_id, provider_id) = fixture
        .seed_snapshot_for_model(&upstream.base_url(), "gpt-5.6")
        .await;
    let server = start_model(&fixture).await;

    let response = post_route(
        server.endpoint(),
        Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap();
    server.shutdown().await.unwrap();

    let database = tokio_rusqlite::Connection::open(fixture.muxvia_home.database_path())
        .await
        .unwrap();
    let observed = database
        .call(|connection| {
            connection.query_row(
                "SELECT request.target, request.provider_id, request.provider_name,
                        request.model, request.protocol, request.outcome, request.http_status,
                        request.input_tokens, request.cached_input_tokens,
                        request.cache_creation_input_tokens, request.output_tokens,
                        request.error_payload IS NULL, pricing.source_model,
                        pricing.estimated_cost_nano_usd
                 FROM request_records request
                 JOIN pricing_snapshots pricing ON pricing.request_record_id = request.id",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, u16>(6)?,
                        row.get::<_, u64>(7)?,
                        row.get::<_, u64>(8)?,
                        row.get::<_, u64>(9)?,
                        row.get::<_, u64>(10)?,
                        row.get::<_, bool>(11)?,
                        row.get::<_, String>(12)?,
                        row.get::<_, u64>(13)?,
                    ))
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(observed.0, "codex");
    assert_eq!(observed.1, provider_id.to_string());
    assert_eq!(observed.2, "Fake upstream");
    assert_eq!(observed.3, "gpt-5.6");
    assert_eq!(observed.4, "openai-responses");
    assert_eq!(observed.5, "success");
    assert_eq!(observed.6, 200);
    assert_eq!(
        (observed.7, observed.8, observed.9, observed.10),
        (100, 20, 0, 30)
    );
    assert!(observed.11, "successful response payload was persisted");
    assert_eq!(observed.12, "gpt-5.6");
    assert!(observed.13 > 0, "priced usage produced a zero estimate");

    upstream.shutdown().await;
}

#[tokio::test]
async fn draining_rejects_new_requests_and_waits_for_an_accepted_response_body() {
    let (upstream, release_upstream_body) =
        FakeUpstream::start_with_blocked_body(StatusCode::OK).await;
    let fixture = StoreFixture::new().await;
    fixture.seed_snapshot(&upstream.base_url()).await;
    let server = start_model(&fixture).await;
    let endpoint = server.endpoint();

    let accepted = timeout(
        Duration::from_secs(1),
        post_route(endpoint, Some(HeaderValue::from_static(ROUTING_CREDENTIAL))),
    )
    .await
    .expect("accepted response head must not wait for its streamed body");
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(server.active_request_count(), 1);

    let drain = server
        .begin_draining()
        .expect("an accepting model listener must enter draining");
    let rejected = post_route(endpoint, Some(HeaderValue::from_static(ROUTING_CREDENTIAL))).await;
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(upstream.state.calls.load(Ordering::SeqCst), 1);
    assert!(
        timeout(Duration::from_millis(100), drain.wait_until_drained())
            .await
            .is_err(),
        "drain completed before the accepted response body was consumed"
    );

    release_upstream_body.send(()).unwrap();
    let body = accepted.bytes().await.unwrap();
    assert!(body.ends_with(b"data: [DONE]\n\n"));
    timeout(Duration::from_secs(1), drain.wait_until_drained())
        .await
        .expect("drain did not complete after the accepted response body");
    assert!(drain.commit());
    server.shutdown().await.unwrap();
    upstream.shutdown().await;
}

struct InvalidatingObservationUpstream {
    database_path: std::path::PathBuf,
}

#[derive(Clone, Copy)]
enum DeterministicFailureKind {
    Semantic,
    Stream,
    JsonSuccess,
}

struct DeterministicFailureUpstream {
    kind: DeterministicFailureKind,
}

#[async_trait]
impl UpstreamTransport for DeterministicFailureUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static(match self.kind {
                DeterministicFailureKind::JsonSuccess => "application/json",
                _ => "text/event-stream",
            }),
        );
        let body = match self.kind {
            DeterministicFailureKind::Semantic => Box::pin(stream::iter([Ok(
                axum::body::Bytes::from_static(b"data: {\"type\":\"response.failed\"}\n\n"),
            )])) as _,
            DeterministicFailureKind::Stream => Box::pin(
                stream::once(async {
                    Ok(axum::body::Bytes::from_static(
                    b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
                    ))
                })
                .chain(stream::once(async {
                    sleep(Duration::from_millis(100)).await;
                    Err(UpstreamError)
                })),
            ) as _,
            DeterministicFailureKind::JsonSuccess => Box::pin(stream::once(async {
                Ok(axum::body::Bytes::from_static(
                    br#"{"id":"response","usage":{"input_tokens":17,"input_tokens_details":{"cached_tokens":2},"output_tokens":6}}"#,
                ))
            })) as _,
        };
        Ok(UpstreamResponse {
            status: StatusCode::OK,
            headers,
            body,
        })
    }
}

#[async_trait]
impl UpstreamTransport for InvalidatingObservationUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let database = tokio_rusqlite::Connection::open(&self.database_path)
            .await
            .unwrap();
        database
            .call(|connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "UPDATE target_route_state SET active_route_plan_id = NULL
                     WHERE target = 'codex'",
                    [],
                )?;
                connection.execute(
                    "DELETE FROM activated_route_plans WHERE target = 'codex'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert("x-upstream", HeaderValue::from_static("observation-failed"));
        Ok(UpstreamResponse {
            status: StatusCode::OK,
            headers,
            body: Box::pin(stream::once(async {
                Ok(axum::body::Bytes::from_static(b"data: [DONE]\n\n"))
            })),
        })
    }
}

#[tokio::test]
async fn successful_upstream_response_survives_serving_observation_failure() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_snapshot("https://unused.example/api/v1/")
        .await;
    let before = fixture.store.target_view().await.unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let reserved = ReservedListener::new(listener).unwrap();
    let server = ModelServer::bind_reserved(
        reserved,
        Arc::clone(&fixture.store),
        Arc::new(InvalidatingObservationUpstream {
            database_path: fixture.muxvia_home.database_path().to_path_buf(),
        }),
    )
    .await
    .unwrap();

    let response = post_route(
        server.endpoint(),
        Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["x-upstream"], "observation-failed");
    assert_eq!(response.text().await.unwrap(), "data: [DONE]\n\n");
    let after = fixture.store.target_view().await.unwrap();
    assert_eq!(after.serving_provider_id, None);
    assert_eq!(after.view_sequence, before.view_sequence);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn semantic_exhaustion_and_committed_stream_failure_persist_distinct_terminal_outcomes() {
    for (kind, expected_status, expected_outcome) in [
        (
            DeterministicFailureKind::Semantic,
            StatusCode::BAD_GATEWAY,
            "semantic-error",
        ),
        (
            DeterministicFailureKind::Stream,
            StatusCode::OK,
            "stream-error",
        ),
        (
            DeterministicFailureKind::JsonSuccess,
            StatusCode::OK,
            "success",
        ),
    ] {
        let fixture = StoreFixture::new().await;
        let (_snapshot_id, provider_id) = fixture
            .seed_snapshot("https://unused.example/api/v1/")
            .await;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let server = ModelServer::bind_reserved(
            ReservedListener::new(listener).unwrap(),
            Arc::clone(&fixture.store),
            Arc::new(DeterministicFailureUpstream { kind }),
        )
        .await
        .unwrap();

        let response = post_route(
            server.endpoint(),
            Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
        )
        .await;
        assert_eq!(response.status(), expected_status);
        let body_result = response.bytes().await;
        match kind {
            DeterministicFailureKind::Semantic => assert!(body_result.is_ok()),
            DeterministicFailureKind::Stream => assert!(body_result.is_err()),
            DeterministicFailureKind::JsonSuccess => assert!(body_result.is_ok()),
        }
        server.shutdown().await.unwrap();

        let records = fixture.request_records().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, expected_outcome);
        assert_eq!(
            records[0].provider_id.as_deref(),
            Some(provider_id.to_string().as_str())
        );
        assert_eq!(records[0].provider_name.as_deref(), Some("Fake upstream"));
        assert!(records[0].error_payload.is_none());
        assert_eq!(
            records[0].usage,
            matches!(kind, DeterministicFailureKind::JsonSuccess).then_some((15, 2, 0, 6))
        );
    }
}

#[tokio::test]
async fn request_record_storage_failure_does_not_change_the_committed_response() {
    let upstream = FakeUpstream::start(StatusCode::OK).await;
    let fixture = StoreFixture::new().await;
    fixture.seed_snapshot(&upstream.base_url()).await;
    let database = tokio_rusqlite::Connection::open(fixture.muxvia_home.database_path())
        .await
        .unwrap();
    database
        .call(|connection| -> tokio_rusqlite::rusqlite::Result<()> {
            connection.execute_batch(
                "CREATE TRIGGER reject_request_record_insert
                 BEFORE INSERT ON request_records
                 BEGIN
                   SELECT RAISE(ABORT, 'controlled-request-record-failure');
                 END;",
            )
        })
        .await
        .unwrap();
    let server = start_model(&fixture).await;

    let response = post_route(
        server.endpoint(),
        Some(HeaderValue::from_static(ROUTING_CREDENTIAL)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().await.unwrap();
    assert!(body.ends_with(b"data: [DONE]\n\n"));
    server.shutdown().await.unwrap();
    assert!(fixture.request_records().await.is_empty());

    upstream.shutdown().await;
}

#[tokio::test]
async fn dropping_downstream_connection_drops_upstream_body_stream() {
    let upstream = FakeUpstream::start(StatusCode::OK).await;
    let fixture = StoreFixture::new().await;
    fixture.seed_snapshot(&upstream.base_url()).await;
    let server = start_model(&fixture).await;

    let mut socket = TcpStream::connect(server.endpoint()).await.unwrap();
    let request = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: {}\r\nX-Muxvia-Routing-Credential: {}\r\nContent-Length: 2\r\n\r\n{{}}",
        server.endpoint(),
        ROUTING_CREDENTIAL
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut received = vec![0; 1024];
    let count = timeout(Duration::from_secs(1), socket.read(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&received[..count]).contains("200 OK"));
    drop(socket);

    timeout(Duration::from_secs(1), async {
        while !upstream.state.body_dropped.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("downstream cancellation must drop the upstream response stream");

    server.shutdown().await.unwrap();
    let records = fixture.request_records().await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, "cancelled");
    assert_eq!(records[0].http_status, Some(200));
    assert!(records[0].error_payload.is_none());
    upstream.shutdown().await;
}
