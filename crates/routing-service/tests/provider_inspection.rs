use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use muxvia_routing::{
    control::protocol::{DiscoverySource, DraftCredentialSource},
    home::MuxviaHome,
    service::provider_inspector::{
        DiscoveredModel, InspectionCategory, MAX_DISCOVERED_MODELS, MAX_DISCOVERY_BODY_BYTES,
        ModelDiscoveryResult, ProviderInspector, ReachabilityResult, build_models_url_candidates,
        parse_models_response,
    },
    state::StateStore,
};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
};
use uuid::Uuid;

#[test]
fn model_candidates_preserve_the_pinned_order_without_cross_origin_fallbacks() {
    let cases = [
        (
            "override",
            "https://ignored.example/v1",
            false,
            Some("https://models.example/catalog?version=one"),
            vec!["https://models.example/catalog?version=one"],
        ),
        (
            "full inference URL containing v1",
            "https://api.example/gateway/v1/chat/completions?trace=discarded",
            true,
            None,
            vec!["https://api.example/gateway/v1/models"],
        ),
        (
            "full inference URL without v1",
            "https://api.example/gateway/responses?trace=discarded",
            true,
            None,
            vec!["https://api.example/gateway/v1/models"],
        ),
        (
            "plain root",
            "https://api.example",
            false,
            None,
            vec!["https://api.example/v1/models"],
        ),
        (
            "v1 root",
            "https://api.example/v1/",
            false,
            None,
            vec!["https://api.example/v1/models"],
        ),
        (
            "another version root",
            "https://api.example/v7",
            false,
            None,
            vec![
                "https://api.example/v7/models",
                "https://api.example/v7/v1/models",
            ],
        ),
    ];

    for (name, base, full, override_url, expected) in cases {
        let actual = build_models_url_candidates(base, full, override_url).unwrap();
        assert_eq!(actual, expected, "{name}");
    }
}

#[test]
fn every_compatibility_suffix_uses_the_exact_three_candidate_order() {
    for suffix in [
        "/api/claudecode",
        "/api/anthropic",
        "/apps/anthropic",
        "/api/coding",
        "/claudecode",
        "/anthropic",
        "/step_plan",
        "/coding",
        "/claude",
    ] {
        let base = format!("https://api.example{suffix}");
        assert_eq!(
            build_models_url_candidates(&base, false, None).unwrap(),
            vec![
                format!("{base}/v1/models"),
                "https://api.example/v1/models".to_owned(),
                "https://api.example/models".to_owned(),
            ],
            "suffix {suffix}",
        );
    }
}

#[test]
fn derived_candidates_drop_queries_reject_unsafe_authority_and_deduplicate_exactly() {
    assert_eq!(
        build_models_url_candidates("https://api.example/v1?secret=discarded", false, None)
            .unwrap(),
        vec!["https://api.example/v1/models"],
    );
    assert_eq!(
        build_models_url_candidates("https://user@api.example/v1", false, None),
        Err(InspectionCategory::InvalidEndpoint),
    );
    assert_eq!(
        build_models_url_candidates("https://api.example/v1#fragment", false, None),
        Err(InspectionCategory::InvalidEndpoint),
    );

    let candidates = build_models_url_candidates(
        "https://api.example/api/claudecode?discard=this",
        false,
        None,
    )
    .unwrap();
    assert!(candidates.iter().all(|candidate| {
        let url = url::Url::parse(candidate).unwrap();
        url.origin().ascii_serialization() == "https://api.example" && url.query().is_none()
    }));
}

#[test]
fn parser_returns_a_stable_deduplicated_order_and_distinguishes_empty_from_malformed() {
    let parsed = parse_models_response(
        br#"{"data":[
            {"id":"zeta","owned_by":"owner-z"},
            {"id":"alpha","owned_by":"owner-a","future":true},
            {"id":"zeta","owned_by":"later-owner"},
            {"id":"  "}
        ],"future":"ignored"}"#,
    )
    .unwrap();
    assert_eq!(
        parsed,
        vec![
            DiscoveredModel {
                id: "alpha".to_owned(),
                display_name: Some("owner-a".to_owned()),
            },
            DiscoveredModel {
                id: "zeta".to_owned(),
                display_name: Some("owner-z".to_owned()),
            },
        ],
    );

    for empty in [br#"{}"#.as_slice(), br#"{"data":null}"#, br#"{"data":[]}"#] {
        assert_eq!(parse_models_response(empty).unwrap(), Vec::new());
    }
    for malformed in [
        br#"[]"#.as_slice(),
        br#"{"data":{}}"#,
        br#"{"data":[{}]}"#,
        br#"{"data":[{"id":7}]}"#,
        br#"{"data":[{"id":"ok","owned_by":7}]}"#,
    ] {
        assert_eq!(
            parse_models_response(malformed),
            Err(InspectionCategory::MalformedResponse),
        );
    }
}

#[test]
fn parser_enforces_the_body_and_model_count_bounds() {
    assert_eq!(
        parse_models_response(&vec![b' '; MAX_DISCOVERY_BODY_BYTES + 1]),
        Err(InspectionCategory::ResponseTooLarge),
    );

    let at_limit = serde_json::json!({
        "data": (0..MAX_DISCOVERED_MODELS)
            .map(|index| serde_json::json!({ "id": format!("model-{index:04}") }))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        parse_models_response(&serde_json::to_vec(&at_limit).unwrap())
            .unwrap()
            .len(),
        MAX_DISCOVERED_MODELS,
    );

    let over_limit = serde_json::json!({
        "data": (0..=MAX_DISCOVERED_MODELS)
            .map(|index| serde_json::json!({ "id": format!("model-{index:04}") }))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        parse_models_response(&serde_json::to_vec(&over_limit).unwrap()),
        Err(InspectionCategory::TooManyModels),
    );
}

#[derive(Clone)]
struct HttpReply {
    status: u16,
    body: Vec<u8>,
    header_delay: Duration,
    body_delay: Duration,
}

impl HttpReply {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body: serde_json::to_vec(&body).unwrap(),
            header_delay: Duration::ZERO,
            body_delay: Duration::ZERO,
        }
    }
}

#[derive(Debug)]
struct CapturedRequest {
    path: String,
    headers: BTreeMap<String, String>,
}

struct HttpServer {
    base_url: String,
    requests: mpsc::UnboundedReceiver<CapturedRequest>,
    task: JoinHandle<()>,
}

impl HttpServer {
    async fn start(replies: Vec<HttpReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (request_tx, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let Ok(read) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                }
                let text = String::from_utf8(bytes).unwrap();
                let mut lines = text.split("\r\n");
                let path = lines
                    .next()
                    .unwrap()
                    .split_ascii_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let headers = lines
                    .take_while(|line| !line.is_empty())
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                    .collect();
                let _ = request_tx.send(CapturedRequest { path, headers });

                tokio::time::sleep(reply.header_delay).await;
                let reason = match reply.status {
                    200 => "OK",
                    401 => "Unauthorized",
                    404 => "Not Found",
                    405 => "Method Not Allowed",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    503 => "Service Unavailable",
                    _ => "Response",
                };
                let headers = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.status,
                    reason,
                    reply.body.len(),
                );
                if stream.write_all(headers.as_bytes()).await.is_err() {
                    continue;
                }
                if stream.flush().await.is_err() {
                    continue;
                }
                tokio::time::sleep(reply.body_delay).await;
                let _ = stream.write_all(&reply.body).await;
            }
        });
        Self {
            base_url,
            requests,
            task,
        }
    }

    async fn next_request(&mut self) -> CapturedRequest {
        tokio::time::timeout(Duration::from_secs(1), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct InspectionFixture {
    root: PathBuf,
    home: MuxviaHome,
    store: Arc<StateStore>,
}

impl InspectionFixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("mx-inspect-{}", Uuid::new_v4()));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        Self { root, home, store }
    }

    fn inspector(&self) -> ProviderInspector {
        ProviderInspector::with_timeouts(
            Arc::clone(&self.store),
            Duration::from_millis(60),
            Duration::from_millis(60),
            Duration::from_millis(20),
        )
        .unwrap()
    }

    async fn create_provider(&self, base_url: &str, credential: &str) -> (Uuid, u64) {
        let outcome = self
            .store
            .apply_provider_action(
                Uuid::new_v4(),
                self.store.target_view().await.unwrap().management_revision,
                json!({
                    "kind": "create-provider",
                    "name": "Inspected",
                    "baseUrl": base_url,
                    "model": "model-test",
                    "credential": { "kind": "replace", "value": credential },
                    "presetKey": null,
                }),
            )
            .await
            .unwrap();
        let provider = outcome.view.providers.last().unwrap();
        (provider.id, provider.provider_revision)
    }

    fn database_bytes(&self) -> Vec<u8> {
        fs::read(self.home.database_path()).unwrap()
    }
}

impl Drop for InspectionFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn ephemeral_source(base_url: String, secret: &str) -> DiscoverySource {
    DiscoverySource::Draft {
        base_url,
        credential_source: DraftCredentialSource::Ephemeral {
            value: secret.to_owned(),
        },
    }
}

fn failure_category(result: &ModelDiscoveryResult) -> InspectionCategory {
    match result {
        ModelDiscoveryResult::Failure { failure } => failure.category,
        ModelDiscoveryResult::Success { .. } => panic!("expected discovery failure"),
    }
}

#[tokio::test]
async fn discovery_sends_bearer_and_falls_back_only_after_an_unsupported_endpoint() {
    let fixture = InspectionFixture::new().await;
    let secret = "discovery-secret-sentinel-must-not-escape";
    let raw_body = "raw-upstream-body-must-not-escape";
    let mut server = HttpServer::start(vec![
        HttpReply::json(404, json!({ "error": raw_body })),
        HttpReply::json(
            200,
            json!({ "data": [{ "id": "zeta" }, { "id": "alpha" }] }),
        ),
    ])
    .await;
    let before_view = fixture.store.target_view().await.unwrap();
    let before_database = fixture.database_bytes();

    let result = fixture
        .inspector()
        .discover_models(ephemeral_source(
            format!("{}/api/anthropic", server.base_url),
            secret,
        ))
        .await;

    let ModelDiscoveryResult::Success {
        models, attempts, ..
    } = &result
    else {
        panic!("unexpected result: {result:?}");
    };
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(*attempts, 2);
    let first = server.next_request().await;
    let second = server.next_request().await;
    assert_eq!(first.path, "/api/anthropic/v1/models");
    assert_eq!(second.path, "/v1/models");
    assert_eq!(
        first.headers.get("authorization").map(String::as_str),
        Some("Bearer discovery-secret-sentinel-must-not-escape")
    );
    assert_eq!(
        second.headers.get("authorization"),
        first.headers.get("authorization")
    );
    assert!(!format!("{result:?}").contains(secret));
    assert!(!format!("{result:?}").contains(raw_body));
    assert_eq!(fixture.store.target_view().await.unwrap(), before_view);
    assert_eq!(fixture.database_bytes(), before_database);

    let mut method_server = HttpServer::start(vec![
        HttpReply::json(405, json!({ "error": "not-a-model-endpoint" })),
        HttpReply::json(200, json!({ "data": [] })),
    ])
    .await;
    let method_result = fixture
        .inspector()
        .discover_models(ephemeral_source(
            format!("{}/api/anthropic", method_server.base_url),
            secret,
        ))
        .await;
    let ModelDiscoveryResult::Success { attempts, .. } = method_result else {
        panic!("unexpected 405 fallback result: {method_result:?}");
    };
    assert_eq!(attempts, 2);
    assert_eq!(
        method_server.next_request().await.path,
        "/api/anthropic/v1/models"
    );
    assert_eq!(method_server.next_request().await.path, "/v1/models");
}

#[tokio::test]
async fn discovery_terminal_paths_are_stable_redacted_and_never_fall_through() {
    let fixture = InspectionFixture::new().await;
    for (status, expected) in [
        (401, InspectionCategory::AuthenticationRejected),
        (429, InspectionCategory::RateLimited),
        (500, InspectionCategory::UpstreamStatus),
    ] {
        let mut server = HttpServer::start(vec![HttpReply::json(
            status,
            json!({ "secret": "status-body-must-not-escape" }),
        )])
        .await;
        let result = fixture
            .inspector()
            .discover_models(ephemeral_source(
                format!("{}/api/anthropic", server.base_url),
                "terminal-secret-must-not-escape",
            ))
            .await;
        assert_eq!(failure_category(&result), expected);
        let ModelDiscoveryResult::Failure { failure } = &result else {
            unreachable!();
        };
        assert_eq!(failure.attempts, 1, "status {status} fell through");
        assert!(!format!("{result:?}").contains("must-not-escape"));
        let _ = server.next_request().await;
    }

    let mut malformed = HttpServer::start(vec![HttpReply::json(
        200,
        json!({ "data": [{ "missing": "id" }] }),
    )])
    .await;
    let result = fixture
        .inspector()
        .discover_models(ephemeral_source(
            format!("{}/api/anthropic", malformed.base_url),
            "parse-secret-must-not-escape",
        ))
        .await;
    assert_eq!(
        failure_category(&result),
        InspectionCategory::MalformedResponse
    );
    let _ = malformed.next_request().await;

    let mut timeout = HttpServer::start(vec![HttpReply {
        status: 200,
        body: serde_json::to_vec(&json!({ "data": [] })).unwrap(),
        header_delay: Duration::from_millis(200),
        body_delay: Duration::ZERO,
    }])
    .await;
    let result = fixture
        .inspector()
        .discover_models(ephemeral_source(
            format!("{}/api/anthropic", timeout.base_url),
            "timeout-secret-must-not-escape",
        ))
        .await;
    assert_eq!(failure_category(&result), InspectionCategory::Timeout);
    let _ = timeout.next_request().await;
    assert!(!format!("{result:?}").contains("must-not-escape"));
}

#[tokio::test]
async fn discovery_streams_into_the_body_cap_and_resolves_saved_credentials_read_only() {
    let fixture = InspectionFixture::new().await;
    let secret = "saved-discovery-secret-must-not-escape";
    let mut server = HttpServer::start(vec![HttpReply {
        status: 200,
        body: vec![b'x'; MAX_DISCOVERY_BODY_BYTES + 1],
        header_delay: Duration::ZERO,
        body_delay: Duration::ZERO,
    }])
    .await;
    let (provider_id, provider_revision) = fixture
        .create_provider(&format!("{}/v1", server.base_url), secret)
        .await;
    let before_view = fixture.store.target_view().await.unwrap();
    let before_database = fixture.database_bytes();

    let result = fixture
        .inspector()
        .discover_models(DiscoverySource::Saved {
            provider_id,
            provider_revision,
        })
        .await;

    assert_eq!(
        failure_category(&result),
        InspectionCategory::ResponseTooLarge
    );
    let request = server.next_request().await;
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer saved-discovery-secret-must-not-escape")
    );
    assert!(!format!("{result:?}").contains(secret));
    assert_eq!(fixture.store.target_view().await.unwrap(), before_view);
    assert_eq!(fixture.database_bytes(), before_database);
}

#[tokio::test]
async fn draft_discovery_can_reuse_a_saved_credential_without_using_the_saved_endpoint() {
    let fixture = InspectionFixture::new().await;
    let secret = "draft-saved-secret-must-not-escape";
    let (provider_id, provider_revision) = fixture
        .create_provider("https://saved-endpoint.invalid/v1", secret)
        .await;
    let mut draft_server = HttpServer::start(vec![HttpReply::json(
        200,
        json!({ "data": [{ "id": "draft-model" }] }),
    )])
    .await;
    let before_view = fixture.store.target_view().await.unwrap();
    let before_database = fixture.database_bytes();

    let result = fixture
        .inspector()
        .discover_models(DiscoverySource::Draft {
            base_url: format!("{}/v1", draft_server.base_url),
            credential_source: DraftCredentialSource::Saved {
                provider_id,
                provider_revision,
            },
        })
        .await;

    let ModelDiscoveryResult::Success { models, .. } = result else {
        panic!("unexpected result: {result:?}");
    };
    assert_eq!(models[0].id, "draft-model");
    let request = draft_server.next_request().await;
    assert_eq!(request.path, "/v1/models");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer draft-saved-secret-must-not-escape")
    );
    assert_eq!(fixture.store.target_view().await.unwrap(), before_view);
    assert_eq!(fixture.database_bytes(), before_database);

    let missing = fixture
        .inspector()
        .discover_models(DiscoverySource::Draft {
            base_url: String::new(),
            credential_source: DraftCredentialSource::Missing,
        })
        .await;
    assert_eq!(
        failure_category(&missing),
        InspectionCategory::MissingCredential
    );
}

#[tokio::test]
async fn reachability_reads_headers_only_sends_no_auth_and_accepts_every_http_status() {
    let fixture = InspectionFixture::new().await;
    for status in [401, 503, 600] {
        let mut server = HttpServer::start(vec![HttpReply {
            status,
            body: vec![b'x'; 32],
            header_delay: if status == 503 {
                Duration::from_millis(30)
            } else {
                Duration::ZERO
            },
            body_delay: Duration::from_millis(300),
        }])
        .await;
        let secret = "reachability-secret-must-not-escape";
        let (provider_id, provider_revision) = fixture
            .create_provider(&format!("{}/exact/base", server.base_url), secret)
            .await;
        let before_view = fixture.store.target_view().await.unwrap();
        let before_database = fixture.database_bytes();
        let started = Instant::now();

        let result = fixture
            .inspector()
            .check_reachability(provider_id, provider_revision)
            .await;

        assert!(started.elapsed() < Duration::from_millis(200));
        let ReachabilityResult::Reachable {
            http_status: actual_status,
            retry_count,
            slow,
            ..
        } = result
        else {
            panic!("unexpected reachability result: {result:?}");
        };
        assert_eq!(actual_status, status);
        assert_eq!(retry_count, 0);
        assert_eq!(slow, status == 503);
        let request = server.next_request().await;
        assert_eq!(request.path, "/exact/base");
        assert_eq!(
            request.headers.get("accept").map(String::as_str),
            Some("*/*")
        );
        assert_eq!(
            request.headers.get("accept-encoding").map(String::as_str),
            Some("identity")
        );
        assert!(!request.headers.contains_key("authorization"));
        assert_eq!(fixture.store.target_view().await.unwrap(), before_view);
        assert_eq!(fixture.database_bytes(), before_database);
    }
}

#[tokio::test]
async fn reachability_retries_one_timeout_but_never_retries_connect_failure() {
    let fixture = InspectionFixture::new().await;
    let delayed = HttpReply {
        status: 200,
        body: Vec::new(),
        header_delay: Duration::from_millis(200),
        body_delay: Duration::ZERO,
    };
    let mut server = HttpServer::start(vec![delayed.clone(), delayed]).await;
    let (provider_id, provider_revision) = fixture
        .create_provider(&server.base_url, "timeout-probe-secret")
        .await;
    let result = fixture
        .inspector()
        .check_reachability(provider_id, provider_revision)
        .await;
    let ReachabilityResult::Unreachable {
        failure,
        retry_count,
        ..
    } = result
    else {
        panic!("unexpected result: {result:?}");
    };
    assert_eq!(failure.category, InspectionCategory::Timeout);
    assert_eq!(retry_count, 1);
    let _ = server.next_request().await;
    let _ = server.next_request().await;

    let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let refused_url = format!("http://{}", unused.local_addr().unwrap());
    drop(unused);
    let (provider_id, provider_revision) = fixture
        .create_provider(&refused_url, "connect-probe-secret")
        .await;
    let result = fixture
        .inspector()
        .check_reachability(provider_id, provider_revision)
        .await;
    let ReachabilityResult::Unreachable {
        failure,
        retry_count,
        ..
    } = result
    else {
        panic!("unexpected result: {result:?}");
    };
    assert_eq!(failure.category, InspectionCategory::Connect);
    assert_eq!(retry_count, 0);
}
