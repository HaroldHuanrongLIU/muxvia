use std::{
    io::Write,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode},
    response::Response,
    routing::post as axum_post,
};
use flate2::{Compression, write::GzEncoder};
use futures_util::{Stream, StreamExt, stream};
use muxvia_routing::{
    control::protocol::{ProviderAuthentication, Target},
    home::MuxviaHome,
    model::{
        ModelServer, ReqwestUpstream, ReservedListener, UpstreamError, UpstreamRequest,
        UpstreamResponse, UpstreamTransport,
    },
    state::StateStore,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, oneshot},
    time::{Duration, sleep, timeout},
};
use uuid::Uuid;

const CODEX_CREDENTIAL: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CLAUDE_CREDENTIAL: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const PROVIDER_SECRET: &str = "provider-secret-must-not-escape";
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

struct StoreFixture {
    _home: TempDir,
    muxvia_home: MuxviaHome,
    store: Arc<StateStore>,
}

impl StoreFixture {
    async fn new() -> Self {
        let home = TempDir::new().unwrap();
        let muxvia_home = MuxviaHome::from_user_home(home.path());
        let store = Arc::new(StateStore::open(&muxvia_home).await.unwrap());
        let fixture = Self {
            _home: home,
            muxvia_home,
            store,
        };
        fixture.seed_routing_credentials().await;
        fixture
    }

    async fn seed_routing_credentials(&self) {
        let database = tokio_rusqlite::Connection::open(self.muxvia_home.database_path())
            .await
            .unwrap();
        database
            .call(|connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "UPDATE target_route_state SET routing_credential = ?1 WHERE target = 'codex'",
                    [CODEX_CREDENTIAL],
                )?;
                connection.execute(
                    "UPDATE target_route_state SET routing_credential = ?1 WHERE target = 'claude'",
                    [CLAUDE_CREDENTIAL],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn seed_claude_snapshot(
        &self,
        base_url: &str,
        authentication: ProviderAuthentication,
        model: &str,
    ) -> (Uuid, Uuid) {
        let snapshot_id = Uuid::new_v4();
        let provider_id = Uuid::new_v4();
        let plan_id = Uuid::new_v4();
        let plan_epoch = Uuid::new_v4();
        let database = tokio_rusqlite::Connection::open(self.muxvia_home.database_path())
            .await
            .unwrap();
        let base_url = base_url.to_owned();
        let model = model.to_owned();
        let authentication = authentication.to_string();
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                let transaction = connection.transaction()?;
                transaction.execute(
                    "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, 'claude', ?2)",
                    (provider_id.to_string(), PROVIDER_SECRET),
                )?;
                transaction.execute(
                    "INSERT INTO providers
                     (id, target, position, provider_revision, name, base_url, model, protocol,
                      authentication, routing_requirement, credential_id, provenance_kind,
                      provenance_key, generated_owner_id)
                     VALUES (?1, 'claude',
                             (SELECT COALESCE(MAX(position) + 1, 0) FROM providers WHERE target = 'claude'),
                             1, 'Claude upstream', ?2, ?3,
                             'anthropic-messages', ?4, 'direct-compatible', ?1, NULL, NULL, NULL)",
                    (
                        provider_id.to_string(),
                        base_url.clone(),
                        model.clone(),
                        authentication.clone(),
                    ),
                )?;
                transaction.execute(
                    "INSERT INTO activated_snapshots
                     (id, target, provider_id, base_url, model, protocol, authentication,
                      provider_bearer_token, epoch)
                     VALUES (?1, 'claude', ?2, ?3, ?4, 'anthropic-messages', ?5, ?6, ?7)",
                    (
                        snapshot_id.to_string(),
                        provider_id.to_string(),
                        base_url.clone(),
                        model.clone(),
                        authentication.clone(),
                        PROVIDER_SECRET,
                        Uuid::new_v4().to_string(),
                    ),
                )?;
                transaction.execute("DELETE FROM failover_draft_members WHERE target = 'claude'", [])?;
                transaction.execute(
                    "INSERT INTO failover_draft_members (target, position, provider_id, provider_revision)
                     VALUES ('claude', 0, ?1, 1)",
                    [provider_id.to_string()],
                )?;
                transaction.execute(
                    "INSERT INTO activated_route_plans (id, target, epoch, created_revision)
                     VALUES (?1, 'claude', ?2, 0)",
                    (plan_id.to_string(), plan_epoch.to_string()),
                )?;
                transaction.execute(
                    "INSERT INTO activated_route_plan_members
                     (plan_id, position, provider_id, provider_revision, name, base_url, model,
                      protocol, authentication, credential_id, routing_requirement)
                     VALUES (?1, 0, ?2, 1, 'Claude upstream', ?3, ?4,
                             'anthropic-messages', ?5, ?6, 'direct-compatible')",
                    (
                        plan_id.to_string(),
                        provider_id.to_string(),
                        base_url,
                        model,
                        authentication,
                        provider_id.to_string(),
                    ),
                )?;
                transaction.execute(
                    "UPDATE target_route_state
                     SET activated_snapshot_id = ?1, current_provider_id = ?2,
                         active_route_plan_id = ?3
                     WHERE target = 'claude'",
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
}

struct CountingUpstream {
    calls: AtomicUsize,
}

struct CapturedRequest {
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

struct CapturingUpstream {
    requests: Mutex<Vec<CapturedRequest>>,
}

struct StaticUpstream {
    status: StatusCode,
    headers: HeaderMap,
    chunks: Vec<Bytes>,
}

struct BlockingCaptureUpstream {
    captured: Mutex<Option<oneshot::Sender<CapturedRequest>>>,
    response_gate: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl UpstreamTransport for BlockingCaptureUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let captured = CapturedRequest {
            method: request.method,
            url: request.url.to_string(),
            headers: request.headers,
            body: request.body.as_bytes().unwrap().to_vec(),
        };
        if let Some(sender) = self.captured.lock().await.take() {
            let _ = sender.send(captured);
        }
        if let Some(gate) = self.response_gate.lock().await.take() {
            let _ = gate.await;
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Ok(UpstreamResponse {
            status: StatusCode::OK,
            headers,
            body: Box::pin(stream::once(async {
                Ok(Bytes::from_static(b"{\"content\":\"upstream-ok\"}"))
            })),
        })
    }
}

struct InvalidatingObservationUpstream {
    database_path: std::path::PathBuf,
}

type ResponseChunkStream = Pin<Box<dyn Stream<Item = Result<Bytes, UpstreamError>> + Send>>;

struct ObservedResponseStream {
    inner: ResponseChunkStream,
    dropped: Arc<AtomicBool>,
}

impl Stream for ObservedResponseStream {
    type Item = Result<Bytes, UpstreamError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(context)
    }
}

impl Drop for ObservedResponseStream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

struct GatedStreamUpstream {
    first_chunk_gate: Mutex<Option<oneshot::Receiver<()>>>,
    dropped: Arc<AtomicBool>,
    chunks: Vec<Bytes>,
    hang_after_first: bool,
}

#[async_trait]
impl UpstreamTransport for GatedStreamUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let gate = self.first_chunk_gate.lock().await.take();
        let chunks = self.chunks.clone();
        let hang_after_first = self.hang_after_first;
        let body = stream::unfold(
            (0_usize, gate, chunks),
            move |(index, mut gate, chunks)| async move {
                if index == 0
                    && let Some(first_chunk_gate) = gate.take()
                {
                    let _ = first_chunk_gate.await;
                }
                if index == 1 && hang_after_first {
                    std::future::pending::<()>().await;
                }
                let chunk = chunks.get(index)?.clone();
                sleep(Duration::from_millis(25)).await;
                Some((Ok(chunk), (index + 1, gate, chunks)))
            },
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        Ok(UpstreamResponse {
            status: StatusCode::OK,
            headers,
            body: Box::pin(ObservedResponseStream {
                inner: Box::pin(body),
                dropped: Arc::clone(&self.dropped),
            }),
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
                     WHERE target = 'claude'",
                    [],
                )?;
                connection.execute(
                    "DELETE FROM activated_route_plans WHERE target = 'claude'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        Ok(UpstreamResponse {
            status: StatusCode::OK,
            headers,
            body: Box::pin(stream::once(async {
                Ok(Bytes::from_static(b"data: {\"type\":\"message_stop\"}\n\n"))
            })),
        })
    }
}

#[async_trait]
impl UpstreamTransport for StaticUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        Ok(UpstreamResponse {
            status: self.status,
            headers: self.headers.clone(),
            body: Box::pin(stream::iter(self.chunks.clone().into_iter().map(Ok))),
        })
    }
}

#[async_trait]
impl UpstreamTransport for CapturingUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let body = request
            .body
            .as_bytes()
            .expect("Messages adapter must rebuild one buffered identity body")
            .to_vec();
        self.requests.lock().await.push(CapturedRequest {
            method: request.method,
            url: request.url.to_string(),
            headers: request.headers,
            body,
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        Ok(UpstreamResponse {
            status: StatusCode::OK,
            headers,
            body: Box::pin(stream::once(async {
                Ok(Bytes::from_static(b"{\"type\":\"message\",\"content\":[]}"))
            })),
        })
    }
}

#[async_trait]
impl UpstreamTransport for CountingUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(UpstreamError)
    }
}

async fn start_model(
    fixture: &StoreFixture,
    target: Target,
    upstream: Arc<CountingUpstream>,
) -> muxvia_routing::model::ModelServerHandle {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    ModelServer::bind_reserved_for(
        ReservedListener::new(listener).unwrap(),
        target,
        Arc::clone(&fixture.store),
        upstream,
    )
    .await
    .unwrap()
}

async fn start_with_transport(
    fixture: &StoreFixture,
    target: Target,
    upstream: Arc<dyn UpstreamTransport>,
) -> muxvia_routing::model::ModelServerHandle {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    ModelServer::bind_reserved_for(
        ReservedListener::new(listener).unwrap(),
        target,
        Arc::clone(&fixture.store),
        upstream,
    )
    .await
    .unwrap()
}

fn route_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap()
}

async fn post(endpoint: SocketAddr, path: &str, headers: HeaderMap) -> reqwest::Response {
    route_client()
        .request(Method::POST, format!("http://{endpoint}{path}"))
        .headers(headers)
        .body("{}")
        .send()
        .await
        .unwrap()
}

fn claude_authorization() -> (&'static str, String) {
    ("authorization", format!("Bearer {CLAUDE_CREDENTIAL}"))
}

#[tokio::test]
async fn claude_listener_exposes_only_native_messages_posts() {
    let fixture = StoreFixture::new().await;
    let upstream = Arc::new(CountingUpstream {
        calls: AtomicUsize::new(0),
    });
    let server = start_model(&fixture, Target::Claude, Arc::clone(&upstream)).await;

    for path in [
        "/v1/messages",
        "/v1/messages?beta=true",
        "/v1/messages/count_tokens",
    ] {
        let response = post(server.endpoint(), path, HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    assert_eq!(
        route_client()
            .get(format!("http://{}/v1/messages", server.endpoint()))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        post(server.endpoint(), "/v1/responses", HeaderMap::new())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 0);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn claude_authorization_rejects_every_invalid_shape_and_the_codex_credential_identically() {
    let fixture = StoreFixture::new().await;
    let upstream = Arc::new(CountingUpstream {
        calls: AtomicUsize::new(0),
    });
    let server = start_model(&fixture, Target::Claude, Arc::clone(&upstream)).await;

    let mut candidates = vec![
        HeaderMap::new(),
        HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("wrong"),
        )]),
        HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", "x".repeat(63))).unwrap(),
        )]),
        HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", "x".repeat(65))).unwrap(),
        )]),
        HeaderMap::from_iter([(axum::http::header::AUTHORIZATION, {
            let mut value = b"Bearer ".to_vec();
            value.extend_from_slice(&[0xff; 64]);
            HeaderValue::from_bytes(&value).unwrap()
        })]),
        HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {CODEX_CREDENTIAL}")).unwrap(),
        )]),
    ];
    let mut duplicate = HeaderMap::new();
    duplicate.append(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {CLAUDE_CREDENTIAL}")).unwrap(),
    );
    duplicate.append(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {CLAUDE_CREDENTIAL}")).unwrap(),
    );
    candidates.push(duplicate);

    let mut observed = Vec::new();
    for headers in candidates {
        let response = post(server.endpoint(), "/v1/messages", headers).await;
        let status = response.status();
        let mut headers = response.headers().clone();
        headers.remove("date");
        observed.push((status, headers, response.bytes().await.unwrap()));
    }
    assert!(observed.iter().all(|item| item == &observed[0]));
    assert_eq!(observed[0].0, StatusCode::UNAUTHORIZED);
    assert!(!String::from_utf8_lossy(&observed[0].2).contains("credential"));
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 0);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn routing_credentials_do_not_cross_authorize_target_endpoints() {
    let fixture = StoreFixture::new().await;
    let upstream = Arc::new(CountingUpstream {
        calls: AtomicUsize::new(0),
    });
    let claude = start_model(&fixture, Target::Claude, Arc::clone(&upstream)).await;
    let codex = start_model(&fixture, Target::Codex, Arc::clone(&upstream)).await;

    let claude_with_codex = post(
        claude.endpoint(),
        "/v1/messages",
        HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {CODEX_CREDENTIAL}")).unwrap(),
        )]),
    )
    .await;
    let codex_with_claude = post(
        codex.endpoint(),
        "/v1/responses",
        HeaderMap::from_iter([(
            axum::http::HeaderName::from_static("x-muxvia-routing-credential"),
            HeaderValue::from_static(CLAUDE_CREDENTIAL),
        )]),
    )
    .await;

    assert_eq!(claude_with_codex.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(codex_with_claude.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 0);

    claude.shutdown().await.unwrap();
    codex.shutdown().await.unwrap();
}

#[tokio::test]
async fn messages_preserve_native_json_and_query_while_owning_only_top_level_model() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://upstream.example/api/v1/",
            ProviderAuthentication::AnthropicApiKey,
            "claude-snapshot-model",
        )
        .await;
    let upstream = Arc::new(CapturingUpstream {
        requests: Mutex::new(Vec::new()),
    });
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::clone(&upstream) as Arc<dyn UpstreamTransport>,
    )
    .await;
    let body: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/claude/messages-tools-thinking.json"
    ))
    .unwrap();

    let response = route_client()
        .post(format!(
            "http://{}/v1/messages?beta=true&trace=one",
            server.endpoint()
        ))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {CLAUDE_CREDENTIAL}"),
        )
        .header("x-api-key", "inbound-must-be-consumed")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "tools-2024-04-04")
        .header("anthropic-beta", "thinking-2025-01-01")
        .header("x-correlation-id", "correlation")
        .header("x-claude-code-future", "kept")
        .header("connection", "x-remove-me")
        .header("x-remove-me", "hop-secret")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body).unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let requests = upstream.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let captured = &requests[0];
    assert_eq!(captured.method, Method::POST);
    assert_eq!(
        captured.url,
        "https://upstream.example/api/v1/messages?beta=true&trace=one"
    );
    assert_eq!(captured.headers["x-api-key"], PROVIDER_SECRET);
    assert!(
        !captured
            .headers
            .contains_key(axum::http::header::AUTHORIZATION)
    );
    assert_eq!(captured.headers.get_all("anthropic-beta").iter().count(), 2);
    assert_eq!(captured.headers["anthropic-version"], "2023-06-01");
    assert_eq!(captured.headers["x-correlation-id"], "correlation");
    assert_eq!(captured.headers["x-claude-code-future"], "kept");
    assert!(!captured.headers.contains_key("x-remove-me"));
    assert!(!captured.headers.contains_key("content-length"));
    let mut expected = body;
    expected["model"] = serde_json::Value::String("claude-snapshot-model".to_owned());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&captured.body).unwrap(),
        expected
    );
    drop(requests);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn count_tokens_uses_native_path_and_explicit_bearer_upstream_authentication() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://upstream.example/v1",
            ProviderAuthentication::AnthropicBearer,
            "claude-count-model",
        )
        .await;
    let upstream = Arc::new(CapturingUpstream {
        requests: Mutex::new(Vec::new()),
    });
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::clone(&upstream) as Arc<dyn UpstreamTransport>,
    )
    .await;

    let response = route_client()
        .post(format!(
            "http://{}/v1/messages/count_tokens?beta=count",
            server.endpoint()
        ))
        .header(claude_authorization().0, claude_authorization().1)
        .header("x-api-key", "inbound-consumed")
        .body(include_bytes!("fixtures/claude/messages-count-tokens.json").as_slice())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let requests = upstream.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://upstream.example/v1/messages/count_tokens?beta=count"
    );
    assert_eq!(
        requests[0].headers[axum::http::header::AUTHORIZATION],
        format!("Bearer {PROVIDER_SECRET}")
    );
    assert!(!requests[0].headers.contains_key("x-api-key"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap()["model"],
        "claude-count-model"
    );
    drop(requests);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn gzip_requests_are_output_bounded_rebuilt_as_identity_json_and_forwarded_once() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://upstream.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-gzip-model",
        )
        .await;
    let upstream = Arc::new(CapturingUpstream {
        requests: Mutex::new(Vec::new()),
    });
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::clone(&upstream) as Arc<dyn UpstreamTransport>,
    )
    .await;
    let original: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/claude/messages-tools-thinking.json"
    ))
    .unwrap();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&serde_json::to_vec(&original).unwrap())
        .unwrap();
    let compressed = encoder.finish().unwrap();

    let response = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .header("content-encoding", "x-gzip")
        .body(compressed)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let requests = upstream.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("content-encoding"));
    assert!(!requests[0].headers.contains_key("content-length"));
    let forwarded: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(forwarded["model"], "claude-gzip-model");
    assert_eq!(
        forwarded["compatible_future_field"],
        original["compatible_future_field"]
    );
    drop(requests);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn body_policy_returns_fixed_local_errors_without_an_upstream_call() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://upstream.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-body-model",
        )
        .await;
    let upstream = Arc::new(CapturingUpstream {
        requests: Mutex::new(Vec::new()),
    });
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::clone(&upstream) as Arc<dyn UpstreamTransport>,
    )
    .await;

    let unsupported = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .header("content-encoding", "br")
        .body("{}")
        .send()
        .await
        .unwrap();
    let stacked = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .header("content-encoding", "gzip, identity")
        .body("{}")
        .send()
        .await
        .unwrap();
    let invalid = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .body("[]")
        .send()
        .await
        .unwrap();
    let malformed = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .body("{")
        .send()
        .await
        .unwrap();
    let invalid_gzip = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .header("content-encoding", "gzip")
        .body("not-gzip")
        .send()
        .await
        .unwrap();
    let streamed_too_large = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .body(reqwest::Body::wrap_stream(stream::iter([
            Ok::<_, std::io::Error>(vec![b' '; MAX_BODY_BYTES / 2]),
            Ok::<_, std::io::Error>(vec![b' '; MAX_BODY_BYTES / 2 + 1]),
        ])))
        .send()
        .await
        .unwrap();
    let mut oversized_encoder = GzEncoder::new(Vec::new(), Compression::fast());
    oversized_encoder
        .write_all(&vec![b' '; MAX_BODY_BYTES + 1])
        .unwrap();
    let decoded_too_large = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .header("content-encoding", "gzip")
        .body(oversized_encoder.finish().unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(unsupported.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(stacked.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        unsupported.bytes().await.unwrap(),
        "unsupported content encoding"
    );
    assert_eq!(
        stacked.bytes().await.unwrap(),
        "unsupported content encoding"
    );
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(invalid.bytes().await.unwrap(), "invalid request body");
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(malformed.bytes().await.unwrap(), "invalid request body");
    assert_eq!(invalid_gzip.status(), StatusCode::BAD_REQUEST);
    assert_eq!(invalid_gzip.bytes().await.unwrap(), "invalid request body");
    assert_eq!(streamed_too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        streamed_too_large.bytes().await.unwrap(),
        "request body too large"
    );
    assert_eq!(decoded_too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        decoded_too_large.bytes().await.unwrap(),
        "request body too large"
    );
    assert!(upstream.requests.lock().await.is_empty());

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn authentication_and_declared_body_limit_reject_before_reading_a_withheld_body() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://unused.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-body-model",
        )
        .await;
    let upstream = Arc::new(CountingUpstream {
        calls: AtomicUsize::new(0),
    });
    let server = start_model(&fixture, Target::Claude, Arc::clone(&upstream)).await;

    for (authorization, content_length, expected_status, expected_body) in [
        (
            "Bearer wrong-routing-credential",
            1024_u64,
            "401 Unauthorized",
            "request rejected",
        ),
        (
            concat!(
                "Bearer ",
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
            ),
            (MAX_BODY_BYTES + 1) as u64,
            "413 Payload Too Large",
            "request body too large",
        ),
    ] {
        let mut socket = TcpStream::connect(server.endpoint()).await.unwrap();
        let request = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            server.endpoint(),
            authorization,
            content_length,
        );
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(1), socket.read_to_end(&mut response))
            .await
            .expect("server waited for the declared request body")
            .unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with(&format!("HTTP/1.1 {expected_status}")));
        assert!(response.ends_with(expected_body));
    }

    assert_eq!(upstream.calls.load(Ordering::SeqCst), 0);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_successful_response_head_records_only_claude_serving_and_preserves_stream_bytes() {
    let fixture = StoreFixture::new().await;
    let (_snapshot_id, provider_id) = fixture
        .seed_claude_snapshot(
            "https://upstream.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-stream-model",
        )
        .await;
    let claude_before = fixture.store.target_view_for(Target::Claude).await.unwrap();
    let codex_before = fixture.store.target_view_for(Target::Codex).await.unwrap();
    let mut updates = fixture.store.subscribe_target_views();
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("x-request-id", HeaderValue::from_static("upstream-id"));
    headers.insert("connection", HeaderValue::from_static("x-remove-response"));
    headers.insert("x-remove-response", HeaderValue::from_static("hop-secret"));
    let chunks = vec![
        Bytes::from_static(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n"),
        Bytes::from_static(
            b"event: content_block_delta\ndata: {\"delta\":{\"text\":\"hello\"}}\n\n",
        ),
        Bytes::from_static(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
    ];
    let expected = chunks.concat();
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::new(StaticUpstream {
            status: StatusCode::OK,
            headers,
            chunks,
        }),
    )
    .await;

    let response = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["x-request-id"], "upstream-id");
    assert!(!response.headers().contains_key("x-remove-response"));
    assert_eq!(response.bytes().await.unwrap(), expected);
    let published = timeout(Duration::from_secs(1), updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(published.target, Target::Claude);
    assert_eq!(published.serving_provider_id, Some(provider_id.to_string()));
    let claude_after = fixture.store.target_view_for(Target::Claude).await.unwrap();
    let codex_after = fixture.store.target_view_for(Target::Codex).await.unwrap();
    assert_eq!(
        claude_after.serving_provider_id,
        Some(provider_id.to_string())
    );
    assert_eq!(claude_after.view_sequence, claude_before.view_sequence + 1);
    assert_eq!(
        claude_after.management_revision,
        claude_before.management_revision
    );
    assert_eq!(codex_after, codex_before);

    server.shutdown().await.unwrap();
}

#[test]
fn claude_golden_manifest_covers_every_fixture_with_verified_hash_and_provenance() {
    let manifest: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/claude/manifest.json")).unwrap();
    let fixtures = manifest["fixtures"].as_array().unwrap();
    assert_eq!(fixtures.len(), 3);
    for fixture in fixtures {
        let file = fixture["file"].as_str().unwrap();
        let bytes: &[u8] = match file {
            "messages-tools-thinking.json" => {
                include_bytes!("fixtures/claude/messages-tools-thinking.json")
            }
            "messages-count-tokens.json" => {
                include_bytes!("fixtures/claude/messages-count-tokens.json")
            }
            "messages-stream.sse" => include_bytes!("fixtures/claude/messages-stream.sse"),
            _ => panic!("manifest named an unknown Claude fixture"),
        };
        let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
        let actual = digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(fixture["sha256"].as_str().unwrap(), actual);
        assert!(!fixture["oracleType"].as_str().unwrap().is_empty());
        assert!(fixture["source"].as_str().unwrap().starts_with("https://"));
        assert_eq!(fixture["retrievalDate"], "2026-08-15");
        assert!(!fixture["behavior"].as_str().unwrap().is_empty());
        assert!(fixture.get("muxviaCompatibilityDeviation").is_some());
    }
}

#[tokio::test]
async fn reqwest_preserves_compressed_upstream_error_headers_and_exact_bytes() {
    let fixture = StoreFixture::new().await;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    fixture
        .seed_claude_snapshot(
            &format!("http://{endpoint}/api/v1"),
            ProviderAuthentication::AnthropicApiKey,
            "claude-error-model",
        )
        .await;
    let raw_error =
        br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(raw_error).unwrap();
    let compressed_error = encoder.finish().unwrap();
    let upstream_bytes = compressed_error.clone();
    let upstream_task = tokio::spawn(async move {
        let router = Router::new().route(
            "/api/v1/messages",
            axum_post(move |_: Request<Body>| {
                let bytes = upstream_bytes.clone();
                async move {
                    Response::builder()
                        .status(StatusCode::TOO_MANY_REQUESTS)
                        .header("content-type", "application/json")
                        .header("content-encoding", "gzip")
                        .header("x-upstream-error", "rate-limited")
                        .body(Body::from(bytes))
                        .unwrap()
                }
            }),
        );
        axum::serve(listener, router).await.unwrap();
    });
    let before = fixture.store.target_view_for(Target::Claude).await.unwrap();
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::new(ReqwestUpstream::new().unwrap()),
    )
    .await;

    let response = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .body(r#"{"messages":[]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["content-encoding"], "gzip");
    assert_eq!(response.headers()["x-upstream-error"], "rate-limited");
    assert_eq!(response.bytes().await.unwrap(), compressed_error);
    let after = fixture.store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(after.serving_provider_id, None);
    assert_eq!(after.view_sequence, before.view_sequence);

    server.shutdown().await.unwrap();
    upstream_task.abort();
    let _ = upstream_task.await;
}

#[tokio::test]
async fn reqwest_preserves_compressed_success_and_sse_bytes_without_auto_decompression() {
    let fixture = StoreFixture::new().await;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let (_snapshot, provider_id) = fixture
        .seed_claude_snapshot(
            &format!("http://{endpoint}/api/v1"),
            ProviderAuthentication::AnthropicApiKey,
            "claude-compressed-model",
        )
        .await;
    let golden_sse = include_bytes!("fixtures/claude/messages-stream.sse").to_vec();
    let mut sse_encoder = GzEncoder::new(Vec::new(), Compression::default());
    sse_encoder.write_all(&golden_sse).unwrap();
    let compressed_sse = sse_encoder.finish().unwrap();
    let count_json = br#"{"input_tokens":42}"#.to_vec();
    let mut count_encoder = GzEncoder::new(Vec::new(), Compression::default());
    count_encoder.write_all(&count_json).unwrap();
    let compressed_count = count_encoder.finish().unwrap();
    let upstream_sse = compressed_sse.clone();
    let upstream_count = compressed_count.clone();
    let upstream_task = tokio::spawn(async move {
        let router = Router::new()
            .route(
                "/api/v1/messages",
                axum_post(move |_: Request<Body>| {
                    let bytes = upstream_sse.clone();
                    async move {
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .header("content-encoding", "gzip")
                            .body(Body::from(bytes))
                            .unwrap()
                    }
                }),
            )
            .route(
                "/api/v1/messages/count_tokens",
                axum_post(move |_: Request<Body>| {
                    let bytes = upstream_count.clone();
                    async move {
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "application/json")
                            .header("content-encoding", "gzip")
                            .body(Body::from(bytes))
                            .unwrap()
                    }
                }),
            );
        axum::serve(listener, router).await.unwrap();
    });
    let before = fixture.store.target_view_for(Target::Claude).await.unwrap();
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::new(ReqwestUpstream::new().unwrap()),
    )
    .await;

    let sse_response = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(sse_response.headers()["content-encoding"], "gzip");
    assert_eq!(sse_response.bytes().await.unwrap(), compressed_sse);

    let count_response = route_client()
        .post(format!(
            "http://{}/v1/messages/count_tokens",
            server.endpoint()
        ))
        .header(claude_authorization().0, claude_authorization().1)
        .body(include_bytes!("fixtures/claude/messages-count-tokens.json").as_slice())
        .send()
        .await
        .unwrap();
    assert_eq!(count_response.headers()["content-encoding"], "gzip");
    assert_eq!(count_response.bytes().await.unwrap(), compressed_count);
    let after = fixture.store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(after.serving_provider_id, Some(provider_id.to_string()));
    assert_eq!(after.view_sequence, before.view_sequence + 2);

    server.shutdown().await.unwrap();
    upstream_task.abort();
    let _ = upstream_task.await;
}

#[tokio::test]
async fn request_pins_one_claude_snapshot_across_a_concurrent_switch() {
    let fixture = StoreFixture::new().await;
    let (_first_snapshot, first_provider) = fixture
        .seed_claude_snapshot(
            "https://first.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-first-model",
        )
        .await;
    let before = fixture.store.target_view_for(Target::Claude).await.unwrap();
    let (captured_tx, captured_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::new(BlockingCaptureUpstream {
            captured: Mutex::new(Some(captured_tx)),
            response_gate: Mutex::new(Some(release_rx)),
        }),
    )
    .await;
    let endpoint = server.endpoint();
    let request = tokio::spawn(async move {
        route_client()
            .post(format!("http://{endpoint}/v1/messages"))
            .header(claude_authorization().0, claude_authorization().1)
            .body(r#"{"model":"client","messages":[]}"#)
            .send()
            .await
            .unwrap()
    });
    let captured = timeout(Duration::from_secs(1), captured_rx)
        .await
        .unwrap()
        .unwrap();
    let (_second_snapshot, second_provider) = fixture
        .seed_claude_snapshot(
            "https://second.example/v1",
            ProviderAuthentication::AnthropicBearer,
            "claude-second-model",
        )
        .await;
    release_tx.send(()).unwrap();

    let response = request.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap(),
        "{\"content\":\"upstream-ok\"}"
    );
    assert_eq!(captured.url, "https://first.example/v1/messages");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&captured.body).unwrap()["model"],
        "claude-first-model"
    );
    assert_eq!(captured.headers["x-api-key"], PROVIDER_SECRET);
    let after = fixture.store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(after.current_provider_id, Some(second_provider.to_string()));
    assert_eq!(
        after.activated_snapshot.as_ref().unwrap().provider_id,
        second_provider
    );
    assert_eq!(after.serving_provider_id, Some(first_provider.to_string()));
    assert_eq!(after.view_sequence, before.view_sequence + 1);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn serving_observation_failure_cannot_replace_an_upstream_response() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://unused.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-observation-model",
        )
        .await;
    let before = fixture.store.target_view_for(Target::Claude).await.unwrap();
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::new(InvalidatingObservationUpstream {
            database_path: fixture.muxvia_home.database_path().to_path_buf(),
        }),
    )
    .await;

    let response = route_client()
        .post(format!("http://{}/v1/messages", server.endpoint()))
        .header(claude_authorization().0, claude_authorization().1)
        .body(r#"{"messages":[]}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(
        response.text().await.unwrap(),
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let after = fixture.store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(after.serving_provider_id, None);
    assert_eq!(after.view_sequence, before.view_sequence);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn downstream_cancellation_during_body_buffering_never_starts_upstream_work() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://unused.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-cancel-model",
        )
        .await;
    let upstream = Arc::new(CountingUpstream {
        calls: AtomicUsize::new(0),
    });
    let server = start_model(&fixture, Target::Claude, Arc::clone(&upstream)).await;
    let mut socket = TcpStream::connect(server.endpoint()).await.unwrap();
    let request = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 1024\r\n\r\n{{",
        server.endpoint(),
        CLAUDE_CREDENTIAL
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    drop(socket);
    sleep(Duration::from_millis(100)).await;

    assert_eq!(upstream.calls.load(Ordering::SeqCst), 0);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn chunked_request_upload_is_buffered_until_eof_then_forwarded_exactly_once() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://upstream.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-upload-model",
        )
        .await;
    let upstream = Arc::new(CapturingUpstream {
        requests: Mutex::new(Vec::new()),
    });
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::clone(&upstream) as Arc<dyn UpstreamTransport>,
    )
    .await;
    let (finish_tx, finish_rx) = oneshot::channel::<()>();
    let upload = stream::unfold((0_u8, Some(finish_rx)), |(step, finish)| async move {
        match step {
            0 => Some((
                Ok::<_, std::io::Error>(b"{\"messages\":[".as_slice()),
                (1, finish),
            )),
            1 => {
                let _ = finish.unwrap().await;
                Some((
                    Ok::<_, std::io::Error>(b"],\"future\":true}".as_slice()),
                    (2, None),
                ))
            }
            _ => None,
        }
    });
    let endpoint = server.endpoint();
    let request = tokio::spawn(async move {
        route_client()
            .post(format!("http://{endpoint}/v1/messages"))
            .header(claude_authorization().0, claude_authorization().1)
            .body(reqwest::Body::wrap_stream(upload))
            .send()
            .await
            .unwrap()
    });
    sleep(Duration::from_millis(50)).await;
    assert!(upstream.requests.lock().await.is_empty());
    finish_tx.send(()).unwrap();

    assert_eq!(request.await.unwrap().status(), StatusCode::OK);
    let requests = upstream.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let forwarded: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(forwarded["model"], "claude-upload-model");
    assert_eq!(forwarded["future"], true);
    drop(requests);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn sse_head_and_chunks_stream_in_golden_byte_order_without_full_response_buffering() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://unused.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-sse-model",
        )
        .await;
    let golden = include_bytes!("fixtures/claude/messages-stream.sse").as_slice();
    let first_end = golden.len() / 3;
    let second_end = first_end * 2;
    let chunks = vec![
        Bytes::copy_from_slice(&golden[..first_end]),
        Bytes::copy_from_slice(&golden[first_end..second_end]),
        Bytes::copy_from_slice(&golden[second_end..]),
    ];
    let (release_tx, release_rx) = oneshot::channel();
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::new(GatedStreamUpstream {
            first_chunk_gate: Mutex::new(Some(release_rx)),
            dropped: Arc::new(AtomicBool::new(false)),
            chunks,
            hang_after_first: false,
        }),
    )
    .await;

    let response = timeout(
        Duration::from_secs(1),
        route_client()
            .post(format!("http://{}/v1/messages", server.endpoint()))
            .header(claude_authorization().0, claude_authorization().1)
            .body(r#"{"messages":[],"stream":true}"#)
            .send(),
    )
    .await
    .expect("response head must not wait for the first SSE chunk")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.bytes_stream();
    release_tx.send(()).unwrap();
    let first = timeout(Duration::from_secs(1), body.next())
        .await
        .expect("released first chunk must arrive")
        .unwrap()
        .unwrap();
    let mut observed = first.to_vec();
    while let Some(chunk) = body.next().await {
        observed.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(observed, golden);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn downstream_disconnect_drops_the_upstream_sse_stream_without_a_detached_pump() {
    let fixture = StoreFixture::new().await;
    fixture
        .seed_claude_snapshot(
            "https://unused.example/v1",
            ProviderAuthentication::AnthropicApiKey,
            "claude-cancel-stream-model",
        )
        .await;
    let dropped = Arc::new(AtomicBool::new(false));
    let server = start_with_transport(
        &fixture,
        Target::Claude,
        Arc::new(GatedStreamUpstream {
            first_chunk_gate: Mutex::new(None),
            dropped: Arc::clone(&dropped),
            chunks: vec![Bytes::from_static(b"data: first\n\n")],
            hang_after_first: true,
        }),
    )
    .await;
    let mut socket = TcpStream::connect(server.endpoint()).await.unwrap();
    let request = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 29\r\n\r\n{{\"messages\":[],\"stream\":true}}",
        server.endpoint(),
        CLAUDE_CREDENTIAL
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut received = vec![0_u8; 1024];
    let count = timeout(
        Duration::from_secs(1),
        tokio::io::AsyncReadExt::read(&mut socket, &mut received),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(String::from_utf8_lossy(&received[..count]).contains("200 OK"));
    drop(socket);

    timeout(Duration::from_secs(1), async {
        while !dropped.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("disconnect must drop the upstream body stream");

    server.shutdown().await.unwrap();
}
