use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use uuid::Uuid;

use crate::{
    control::protocol::Target,
    state::{ActivatedRoutePlanSnapshot, RoutePlanMemberSnapshot, StateStore},
};

use super::{
    UpstreamRequest, UpstreamResponse, UpstreamTransport,
    commitment::{PrimedResponse, prime_success_response},
};

pub(crate) struct RoutedUpstream {
    pub(crate) response: UpstreamResponse,
    pub(crate) provider_id: Uuid,
}

pub(crate) struct PinnedRouteResult {
    pub(crate) plan_id: Uuid,
    pub(crate) plan_epoch: Uuid,
    pub(crate) routed: Option<RoutedUpstream>,
    pub(crate) observations: Vec<RouteHealthObservation>,
}

pub(crate) struct RouteHealthObservation {
    pub(crate) provider_id: Uuid,
    pub(crate) state: &'static str,
    pub(crate) consecutive_successes: u64,
    pub(crate) consecutive_failures: u64,
    pub(crate) total_attempts: u64,
    pub(crate) failed_attempts: u64,
    pub(crate) outcome: &'static str,
}

pub(crate) struct RouteHealthRuntime {
    targets: [Mutex<HashMap<Uuid, Circuit>>; 2],
}

impl Default for RouteHealthRuntime {
    fn default() -> Self {
        Self {
            targets: std::array::from_fn(|_| Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Default)]
struct Circuit {
    phase: CircuitPhase,
    consecutive_successes: u64,
    consecutive_failures: u64,
    total_attempts: u64,
    failed_attempts: u64,
}

#[derive(Default)]
enum CircuitPhase {
    #[default]
    Closed,
    Open {
        since: Instant,
    },
    HalfOpen {
        successes: u8,
        in_flight: bool,
    },
}

impl RouteHealthRuntime {
    fn target(&self, target: Target) -> &Mutex<HashMap<Uuid, Circuit>> {
        &self.targets[match target {
            Target::Codex => 0,
            Target::Claude => 1,
        }]
    }

    fn admit(
        &self,
        target: Target,
        provider_id: Uuid,
        now: Instant,
    ) -> Option<RouteAttemptPermit<'_>> {
        let mut circuits = self.target(target).lock().unwrap();
        let circuit = circuits.entry(provider_id).or_default();
        let half_open = match &mut circuit.phase {
            CircuitPhase::Closed => false,
            CircuitPhase::Open { since }
                if now.duration_since(*since) < Duration::from_secs(60) =>
            {
                return None;
            }
            CircuitPhase::Open { .. } => {
                circuit.phase = CircuitPhase::HalfOpen {
                    successes: 0,
                    in_flight: true,
                };
                true
            }
            CircuitPhase::HalfOpen { in_flight, .. } if *in_flight => return None,
            CircuitPhase::HalfOpen { in_flight, .. } => {
                *in_flight = true;
                true
            }
        };
        Some(RouteAttemptPermit {
            health: self,
            target,
            provider_id,
            half_open,
            completed: false,
        })
    }

    fn success(&self, target: Target, provider_id: Uuid) -> RouteHealthObservation {
        let mut circuits = self.target(target).lock().unwrap();
        let circuit = circuits.entry(provider_id).or_default();
        circuit.total_attempts += 1;
        circuit.consecutive_successes += 1;
        circuit.consecutive_failures = 0;
        let state = match &mut circuit.phase {
            CircuitPhase::HalfOpen {
                successes,
                in_flight,
            } => {
                *in_flight = false;
                *successes += 1;
                if *successes >= 2 {
                    circuit.phase = CircuitPhase::Closed;
                    "healthy"
                } else {
                    "degraded"
                }
            }
            _ => {
                circuit.phase = CircuitPhase::Closed;
                "healthy"
            }
        };
        observation(provider_id, circuit, state, "success")
    }

    fn failure(
        &self,
        target: Target,
        provider_id: Uuid,
        now: Instant,
        outcome: &'static str,
    ) -> RouteHealthObservation {
        let mut circuits = self.target(target).lock().unwrap();
        let circuit = circuits.entry(provider_id).or_default();
        circuit.total_attempts += 1;
        circuit.failed_attempts += 1;
        circuit.consecutive_successes = 0;
        circuit.consecutive_failures += 1;
        let error_rate_opens = circuit.total_attempts >= 10
            && circuit.failed_attempts.saturating_mul(10)
                >= circuit.total_attempts.saturating_mul(6);
        let opens = matches!(circuit.phase, CircuitPhase::HalfOpen { .. })
            || circuit.consecutive_failures >= 4
            || error_rate_opens;
        let state = if opens {
            circuit.phase = CircuitPhase::Open { since: now };
            "unavailable"
        } else {
            "degraded"
        };
        observation(provider_id, circuit, state, outcome)
    }
}

struct RouteAttemptPermit<'a> {
    health: &'a RouteHealthRuntime,
    target: Target,
    provider_id: Uuid,
    half_open: bool,
    completed: bool,
}

impl RouteAttemptPermit<'_> {
    fn success(mut self) -> RouteHealthObservation {
        self.completed = true;
        self.health.success(self.target, self.provider_id)
    }

    fn failure(mut self, now: Instant, outcome: &'static str) -> RouteHealthObservation {
        self.completed = true;
        self.health
            .failure(self.target, self.provider_id, now, outcome)
    }
}

impl Drop for RouteAttemptPermit<'_> {
    fn drop(&mut self) {
        if !self.half_open || self.completed {
            return;
        }
        let mut circuits = self.health.target(self.target).lock().unwrap();
        if let Some(Circuit {
            phase: CircuitPhase::HalfOpen { in_flight, .. },
            ..
        }) = circuits.get_mut(&self.provider_id)
        {
            *in_flight = false;
        }
    }
}

fn observation(
    provider_id: Uuid,
    circuit: &Circuit,
    state: &'static str,
    outcome: &'static str,
) -> RouteHealthObservation {
    RouteHealthObservation {
        provider_id,
        state,
        consecutive_successes: circuit.consecutive_successes,
        consecutive_failures: circuit.consecutive_failures,
        total_attempts: circuit.total_attempts,
        failed_attempts: circuit.failed_attempts,
        outcome,
    }
}

pub(crate) async fn pin_route_plan(
    store: &StateStore,
    target: Target,
) -> Option<ActivatedRoutePlanSnapshot> {
    store.activated_route_plan_for(target).await.ok()?
}

pub(crate) async fn route_pinned_plan(
    plan: ActivatedRoutePlanSnapshot,
    target: Target,
    health: &RouteHealthRuntime,
    upstream: &Arc<dyn UpstreamTransport>,
    build: impl Fn(&RoutePlanMemberSnapshot) -> Option<UpstreamRequest>,
) -> PinnedRouteResult {
    let plan_id = plan.id;
    let plan_epoch = plan.epoch;
    let member_count = plan.members.len();
    let mut last_response = None;
    let mut observations = Vec::new();
    for (index, member) in plan.members.iter().enumerate() {
        let Some(attempt) = health.admit(target, member.provider_id, Instant::now()) else {
            continue;
        };
        let Some(request) = build(member) else {
            observations.push(attempt.failure(Instant::now(), "configuration-failure"));
            continue;
        };
        let mut response = match upstream.send(request).await {
            Ok(response) => response,
            Err(_) => {
                observations.push(attempt.failure(Instant::now(), "transport-failure"));
                continue;
            }
        };
        if response.status.is_success() {
            response = match prime_success_response(response, target).await {
                PrimedResponse::Committed(response) => response,
                PrimedResponse::Retry => {
                    observations.push(attempt.failure(Instant::now(), "semantic-failure"));
                    continue;
                }
            };
        }
        let routed = RoutedUpstream {
            response,
            provider_id: member.provider_id,
        };
        if routed.response.status.is_success() {
            observations.push(attempt.success());
        } else if retryable_status(routed.response.status) {
            observations.push(attempt.failure(Instant::now(), "retryable-upstream-status"));
        }
        if !retryable_status(routed.response.status) || index + 1 == member_count {
            return PinnedRouteResult {
                plan_id,
                plan_epoch,
                routed: Some(routed),
                observations,
            };
        }
        last_response = Some(routed);
    }
    PinnedRouteResult {
        plan_id,
        plan_epoch,
        routed: last_response,
        observations,
    }
}

fn retryable_status(status: StatusCode) -> bool {
    let code = status.as_u16();
    matches!(code, 401 | 403 | 404 | 408 | 409 | 429 | 451)
        || ((500..=599).contains(&code) && code != 501)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use axum::{
        body::Bytes,
        http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    };
    use futures_util::stream;
    use reqwest::{Body, Url};
    use secrecy::SecretString;

    use super::{RouteHealthRuntime, retryable_status, route_pinned_plan};
    use crate::{
        control::protocol::{ProviderAuthentication, ProviderProtocol, Target},
        model::{UpstreamError, UpstreamRequest, UpstreamResponse, UpstreamTransport},
        state::{ActivatedRoutePlanSnapshot, RoutePlanMemberSnapshot},
    };
    use uuid::Uuid;

    #[test]
    fn retryable_statuses_match_the_fixed_release_policy() {
        assert!(!retryable_status(StatusCode::OK));
        for code in [
            302, 400, 402, 405, 406, 410, 413, 414, 415, 418, 422, 499, 501,
        ] {
            assert!(!retryable_status(StatusCode::from_u16(code).unwrap()));
        }
        for code in [401, 403, 404, 408, 409, 429, 451, 500, 502, 503, 504] {
            assert!(retryable_status(StatusCode::from_u16(code).unwrap()));
        }
    }

    struct SequencedUpstream {
        outcomes: Mutex<VecDeque<Result<StatusCode, ()>>>,
        urls: Mutex<Vec<String>>,
    }

    type SemanticBody = Vec<Result<&'static [u8], ()>>;

    struct SemanticUpstream {
        bodies: Mutex<VecDeque<SemanticBody>>,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl UpstreamTransport for SemanticUpstream {
        async fn send(&self, _request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
            *self.calls.lock().unwrap() += 1;
            let chunks = self.bodies.lock().unwrap().pop_front().unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            Ok(UpstreamResponse {
                status: StatusCode::OK,
                headers,
                body: Box::pin(stream::iter(
                    chunks
                        .into_iter()
                        .map(|chunk| chunk.map(Bytes::from_static).map_err(|_| UpstreamError)),
                )),
            })
        }
    }

    #[async_trait]
    impl UpstreamTransport for SequencedUpstream {
        async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
            self.urls.lock().unwrap().push(request.url.to_string());
            match self.outcomes.lock().unwrap().pop_front().unwrap() {
                Ok(status) => Ok(UpstreamResponse {
                    status,
                    headers: {
                        let mut headers = HeaderMap::new();
                        headers.insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        );
                        headers
                    },
                    body: Box::pin(stream::iter([Ok(Bytes::from_static(b"{\"ok\":true}"))])),
                }),
                Err(()) => Err(UpstreamError),
            }
        }
    }

    fn plan() -> ActivatedRoutePlanSnapshot {
        ActivatedRoutePlanSnapshot {
            id: Uuid::new_v4(),
            epoch: Uuid::new_v4(),
            members: ["primary", "fallback"]
                .into_iter()
                .map(|name| RoutePlanMemberSnapshot {
                    provider_id: Uuid::new_v4(),
                    base_url: format!("https://{name}.test/v1"),
                    model: format!("{name}-model"),
                    provider_credential: Some(SecretString::from(format!("{name}-secret"))),
                    protocol: ProviderProtocol::OpenaiResponses,
                    authentication: ProviderAuthentication::OpenaiBearer,
                    subscription_binding: None,
                })
                .collect(),
        }
    }

    fn request_for(member: &RoutePlanMemberSnapshot) -> Option<UpstreamRequest> {
        Some(UpstreamRequest {
            method: Method::POST,
            url: Url::parse(&format!("{}/responses", member.base_url)).ok()?,
            headers: HeaderMap::new(),
            body: Body::from("request"),
        })
    }

    #[tokio::test]
    async fn retryable_failure_uses_fallback_but_next_request_fails_back_to_primary() {
        let plan = plan();
        let second_plan = plan.clone();
        let primary_id = plan.members[0].provider_id;
        let fallback_id = plan.members[1].provider_id;
        let upstream: Arc<dyn UpstreamTransport> = Arc::new(SequencedUpstream {
            outcomes: Mutex::new(VecDeque::from([
                Ok(StatusCode::SERVICE_UNAVAILABLE),
                Ok(StatusCode::OK),
                Ok(StatusCode::OK),
            ])),
            urls: Mutex::new(Vec::new()),
        });
        let health = RouteHealthRuntime::default();
        let first = route_pinned_plan(plan, Target::Codex, &health, &upstream, request_for)
            .await
            .routed
            .unwrap();
        assert_eq!(first.provider_id, fallback_id);

        let second = route_pinned_plan(second_plan, Target::Codex, &health, &upstream, request_for)
            .await
            .routed
            .unwrap();
        assert_eq!(
            second.provider_id, primary_id,
            "a later request did not fail back"
        );
    }

    #[tokio::test]
    async fn nonretryable_client_status_does_not_attempt_a_fallback() {
        let plan = plan();
        let primary_id = plan.members[0].provider_id;
        let upstream = Arc::new(SequencedUpstream {
            outcomes: Mutex::new(VecDeque::from([
                Ok(StatusCode::BAD_REQUEST),
                Ok(StatusCode::OK),
            ])),
            urls: Mutex::new(Vec::new()),
        });
        let dynamic: Arc<dyn UpstreamTransport> = upstream.clone();
        let routed = route_pinned_plan(
            plan,
            Target::Codex,
            &RouteHealthRuntime::default(),
            &dynamic,
            request_for,
        )
        .await
        .routed
        .unwrap();
        assert_eq!(routed.provider_id, primary_id);
        assert_eq!(upstream.urls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn semantic_failure_before_output_fails_over_but_committed_output_never_retries() {
        use futures_util::StreamExt;

        let first_plan = plan();
        let fallback_id = first_plan.members[1].provider_id;
        let upstream = Arc::new(SemanticUpstream {
            bodies: Mutex::new(VecDeque::from([
                vec![
                    Ok(b"data: {\"type\":\"response.created\"}\n\n".as_slice()),
                    Ok(b"data: {\"type\":\"response.failed\"}\n\n".as_slice()),
                ],
                vec![
                    Ok(b"data: {\"type\":\"response.created\"}\n\n".as_slice()),
                    Ok(
                        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n"
                            .as_slice(),
                    ),
                ],
            ])),
            calls: Mutex::new(0),
        });
        let dynamic: Arc<dyn UpstreamTransport> = upstream.clone();
        let routed = route_pinned_plan(
            first_plan,
            Target::Codex,
            &RouteHealthRuntime::default(),
            &dynamic,
            request_for,
        )
        .await;
        assert_eq!(routed.routed.as_ref().unwrap().provider_id, fallback_id);
        assert_eq!(*upstream.calls.lock().unwrap(), 2);
        assert_eq!(routed.observations[0].outcome, "semantic-failure");

        let committed_plan = plan();
        let primary_id = committed_plan.members[0].provider_id;
        let committed_upstream = Arc::new(SemanticUpstream {
            bodies: Mutex::new(VecDeque::from([
                vec![
                    Ok(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"committed\"}\n\n".as_slice()),
                    Err(()),
                ],
                vec![Ok(b"data: {\"type\":\"response.completed\"}\n\n".as_slice())],
            ])),
            calls: Mutex::new(0),
        });
        let dynamic: Arc<dyn UpstreamTransport> = committed_upstream.clone();
        let mut committed = route_pinned_plan(
            committed_plan,
            Target::Codex,
            &RouteHealthRuntime::default(),
            &dynamic,
            request_for,
        )
        .await
        .routed
        .unwrap();
        assert_eq!(committed.provider_id, primary_id);
        assert!(committed.response.body.next().await.unwrap().is_ok());
        assert!(committed.response.body.next().await.unwrap().is_err());
        assert_eq!(*committed_upstream.calls.lock().unwrap(), 1);
    }

    #[test]
    fn circuit_opens_after_four_failures_and_closes_after_two_half_open_successes() {
        let health = RouteHealthRuntime::default();
        let provider_id = Uuid::new_v4();
        let now = std::time::Instant::now();
        for index in 0..4 {
            let observation = health
                .admit(Target::Codex, provider_id, now)
                .expect("closed circuit must admit the request")
                .failure(now, "transport-failure");
            assert_eq!(
                observation.state,
                if index == 3 {
                    "unavailable"
                } else {
                    "degraded"
                }
            );
        }
        assert!(health.admit(Target::Codex, provider_id, now).is_none());
        let later = now + std::time::Duration::from_secs(60);
        assert_eq!(
            health
                .admit(Target::Codex, provider_id, later)
                .expect("elapsed circuit must admit one probe")
                .success()
                .state,
            "degraded"
        );
        assert_eq!(
            health
                .admit(Target::Codex, provider_id, later)
                .expect("half-open circuit must admit the next probe")
                .success()
                .state,
            "healthy"
        );
        assert!(health.admit(Target::Codex, provider_id, later).is_some());
    }

    #[test]
    fn error_rate_threshold_and_circuit_state_are_target_isolated() {
        let health = RouteHealthRuntime::default();
        let provider_id = Uuid::new_v4();
        let now = std::time::Instant::now();
        for attempt in 0..10 {
            let permit = health
                .admit(Target::Codex, provider_id, now)
                .expect("closed circuit must admit the request");
            if matches!(attempt, 0 | 2 | 4 | 6 | 8 | 9) {
                permit.failure(now, "transport-failure");
            } else {
                permit.success();
            }
        }
        assert!(health.admit(Target::Codex, provider_id, now).is_none());
        assert!(health.admit(Target::Claude, provider_id, now).is_some());

        let half_open = now + std::time::Duration::from_secs(60);
        let _probe = health
            .admit(Target::Codex, provider_id, half_open)
            .expect("elapsed circuit must admit one probe");
        assert!(
            health
                .admit(Target::Codex, provider_id, half_open)
                .is_none()
        );
    }

    #[test]
    fn dropping_a_half_open_attempt_releases_the_single_probe_permit() {
        let health = RouteHealthRuntime::default();
        let provider_id = Uuid::new_v4();
        let now = std::time::Instant::now();
        for _ in 0..4 {
            health
                .admit(Target::Codex, provider_id, now)
                .expect("closed circuit must admit the request")
                .failure(now, "transport-failure");
        }
        let half_open = now + std::time::Duration::from_secs(60);
        let probe = health
            .admit(Target::Codex, provider_id, half_open)
            .expect("elapsed circuit must admit one probe");
        assert!(
            health
                .admit(Target::Codex, provider_id, half_open)
                .is_none()
        );
        drop(probe);
        assert!(
            health
                .admit(Target::Codex, provider_id, half_open)
                .is_some()
        );
    }
}
