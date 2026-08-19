use std::sync::Arc;

use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::{
    control::protocol::{
        ActionStatus, ClaudePreflightContext, ControlProblem, Target, UniversalProviderAction,
    },
    state::{StateStore, UniversalProviderActionFailure, UniversalProviderSynchronizationCommit},
};

use super::{
    reconcile::{ReconciliationRuntime, ReconciliationService},
    reconciliation_adapter::ReconciliationContext,
};

pub(crate) struct ProviderSynchronizationService {
    state: Arc<StateStore>,
    reconciliation: ReconciliationService,
}

pub(crate) struct ProviderSynchronizationAttempt {
    pub(crate) result:
        Result<UniversalProviderSynchronizationCommit, UniversalProviderActionFailure>,
    pub(crate) eligibility_publication: Option<crate::control::protocol::TargetView>,
}

impl ProviderSynchronizationAttempt {
    fn without_publication(
        result: Result<UniversalProviderSynchronizationCommit, UniversalProviderActionFailure>,
    ) -> Self {
        Self {
            result,
            eligibility_publication: None,
        }
    }
}

impl ProviderSynchronizationService {
    pub(crate) fn from_runtime(state: Arc<StateStore>, runtime: ReconciliationRuntime) -> Self {
        Self {
            state: Arc::clone(&state),
            reconciliation: ReconciliationService::from_runtime(state, runtime),
        }
    }

    pub(crate) async fn lock_catalog_lifecycle_mutation(&self) -> [OwnedMutexGuard<()>; 2] {
        let codex = self
            .reconciliation
            .lock_target_mutation(Target::Codex)
            .await;
        let claude = self
            .reconciliation
            .lock_target_mutation(Target::Claude)
            .await;
        [codex, claude]
    }

    pub(crate) async fn apply_raw(
        &self,
        action_id: Uuid,
        expected_revision: u64,
        raw_action: serde_json::Value,
        claude_context: Option<ClaudePreflightContext>,
    ) -> ProviderSynchronizationAttempt {
        match self.state.universal_provider_receipt(action_id).await {
            Ok(Some(mut outcome)) => {
                outcome.status = ActionStatus::Replayed;
                return ProviderSynchronizationAttempt::without_publication(Ok(
                    UniversalProviderSynchronizationCommit {
                        outcome,
                        target_views: Vec::new(),
                    },
                ));
            }
            Ok(None) => {}
            Err(_) => {
                return ProviderSynchronizationAttempt::without_publication(Err(self
                    .failure("state-store-error")
                    .await));
            }
        }
        let (provider_id, provider_revision) =
            match serde_json::from_value::<UniversalProviderAction>(raw_action) {
                Ok(UniversalProviderAction::SynchronizeUniversalProvider {
                    provider_id,
                    provider_revision,
                }) => (provider_id, provider_revision),
                Ok(_) => {
                    return ProviderSynchronizationAttempt::without_publication(Err(self
                        .failure("unsupported-operation")
                        .await));
                }
                Err(_) => {
                    return ProviderSynchronizationAttempt::without_publication(Err(self
                        .failure("invalid-universal-provider")
                        .await));
                }
            };

        let targets = match self
            .state
            .universal_provider_synchronization_targets(provider_id)
            .await
        {
            Ok(targets) => targets,
            Err(_) => {
                return ProviderSynchronizationAttempt::without_publication(Err(self
                    .failure("state-store-error")
                    .await));
            }
        };
        let mut _target_gates = Vec::with_capacity(targets.len());
        for target in &targets {
            _target_gates.push(self.reconciliation.lock_target_mutation(*target).await);
        }
        match self.state.universal_provider_receipt(action_id).await {
            Ok(Some(mut outcome)) => {
                outcome.status = ActionStatus::Replayed;
                return ProviderSynchronizationAttempt::without_publication(Ok(
                    UniversalProviderSynchronizationCommit {
                        outcome,
                        target_views: Vec::new(),
                    },
                ));
            }
            Ok(None) => {}
            Err(_) => {
                return ProviderSynchronizationAttempt::without_publication(Err(self
                    .failure("state-store-error")
                    .await));
            }
        }
        for target in targets {
            let context = match target {
                Target::Codex => Some(ReconciliationContext::Codex),
                Target::Claude => claude_context.clone().map(ReconciliationContext::Claude),
            };
            let allowed = self
                .reconciliation
                .ensure_synchronization_write_allowed(target, context)
                .await;
            if let Err(failure) = allowed.result {
                return ProviderSynchronizationAttempt {
                    result: Err(self.state.universal_provider_failure(failure.problem).await),
                    eligibility_publication: allowed.publication,
                };
            }
        }
        ProviderSynchronizationAttempt::without_publication(
            self.state
                .synchronize_universal_provider_action(
                    action_id,
                    expected_revision,
                    provider_id,
                    provider_revision,
                )
                .await,
        )
    }

    async fn failure(&self, code: &'static str) -> UniversalProviderActionFailure {
        self.state.universal_provider_failure(problem(code)).await
    }
}

fn problem(code: &'static str) -> ControlProblem {
    let message = match code {
        "state-store-error" => "State store operation failed",
        "unsupported-operation" => "Universal Provider action is not supported",
        "invalid-universal-provider" => "Universal Provider action is malformed",
        _ => "Provider synchronization is blocked",
    };
    ControlProblem {
        code: code.to_owned(),
        message: message.to_owned(),
        source: None,
        selector: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        future::Future,
        path::{Path, PathBuf},
        pin::Pin,
        sync::Arc,
    };

    use async_trait::async_trait;
    use axum::{body::Bytes, http::StatusCode};
    use futures_util::stream;
    use secrecy::ExposeSecret;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    use crate::{
        claude::{ClaudeCapability, ClaudeProbe, ClaudeProblem},
        codex::{CodexCapability, CodexProbe, CodexProblem},
        control::protocol::{ProviderAuthentication, Target},
        home::MuxviaHome,
        model::{UpstreamError, UpstreamRequest, UpstreamResponse, UpstreamTransport},
        service::activate::ActivationService,
        state::StateStore,
    };

    use super::{
        super::reconcile::{
            ReconciliationRuntime, ReconciliationService, ReconciliationTargetRuntime,
        },
        ProviderSynchronizationService,
    };

    struct TestedCodexProbe;

    struct NoopUpstream;

    #[async_trait]
    impl UpstreamTransport for NoopUpstream {
        async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
            Ok(UpstreamResponse {
                status: StatusCode::OK,
                headers: Default::default(),
                body: Box::pin(stream::once(async { Ok(Bytes::new()) })),
            })
        }
    }

    struct HeldUpstream {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl HeldUpstream {
        fn new() -> Self {
            Self {
                started: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            }
        }
    }

    #[async_trait]
    impl UpstreamTransport for HeldUpstream {
        async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(UpstreamResponse {
                status: StatusCode::OK,
                headers: Default::default(),
                body: Box::pin(stream::once(async { Ok(Bytes::from_static(b"{}")) })),
            })
        }
    }

    impl CodexProbe for TestedCodexProbe {
        fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
            Ok(CodexCapability::Tested {
                version: "0.1.0".to_owned(),
            })
        }

        fn probe_cancellable<'a>(
            &'a self,
            _: &'a Path,
            _: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>>
        {
            Box::pin(async {
                Ok(CodexCapability::Tested {
                    version: "0.1.0".to_owned(),
                })
            })
        }
    }

    struct UnknownCodexProbe;

    impl CodexProbe for UnknownCodexProbe {
        fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
            Ok(CodexCapability::UnknownCompatible {
                version: "codex-next".to_owned(),
                warning: "newer compatible Codex".to_owned(),
            })
        }

        fn probe_cancellable<'a>(
            &'a self,
            _: &'a Path,
            _: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>>
        {
            Box::pin(async {
                Ok(CodexCapability::UnknownCompatible {
                    version: "codex-next".to_owned(),
                    warning: "newer compatible Codex".to_owned(),
                })
            })
        }
    }

    struct IncompatibleCodexProbe;

    impl CodexProbe for IncompatibleCodexProbe {
        fn probe(&self, executable: &Path) -> Result<CodexCapability, CodexProblem> {
            Err(CodexProblem::new(
                "incompatible-target-cli",
                Some(executable),
            ))
        }

        fn probe_cancellable<'a>(
            &'a self,
            executable: &'a Path,
            _: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>>
        {
            Box::pin(async move {
                Err(CodexProblem::new(
                    "incompatible-target-cli",
                    Some(executable),
                ))
            })
        }
    }

    struct TestedClaudeProbe;

    impl ClaudeProbe for TestedClaudeProbe {
        fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
            Ok(ClaudeCapability::Tested {
                version: "0.1.0".to_owned(),
            })
        }

        fn probe_cancellable<'a>(
            &'a self,
            _: &'a Path,
            _: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<ClaudeCapability, ClaudeProblem>> + Send + 'a>>
        {
            Box::pin(async {
                Ok(ClaudeCapability::Tested {
                    version: "0.1.0".to_owned(),
                })
            })
        }
    }

    #[tokio::test]
    async fn coordinator_materializes_both_targets_after_live_eligibility() {
        let root = std::env::temp_dir().join(format!(
            "muxvia-provider-synchronization-{}",
            uuid::Uuid::new_v4()
        ));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let state = Arc::new(StateStore::open(&home).await.unwrap());
        let created = state
            .apply_universal_provider_action(
                uuid::Uuid::from_u128(0x901),
                0,
                serde_json::json!({
                    "kind": "create-universal-provider",
                    "name": "Coordinator Source",
                    "baseUrl": "https://coordinator.example/v1",
                    "credential": { "kind": "replace", "value": "COORDINATOR_SECRET_88312" },
                    "presetKey": null,
                    "targets": [
                        { "target": "codex", "enabled": true, "model": "codex-model", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                        { "target": "claude", "enabled": true, "model": "claude-model", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                    ]
                }),
            )
            .await
            .unwrap();
        let provider_id = created.view.providers[0].id;
        let provider_revision = created.view.providers[0].provider_revision;
        let runtime = ReconciliationRuntime {
            home: home.clone(),
            codex_probe: Arc::new(TestedCodexProbe),
            claude_probe: Arc::new(TestedClaudeProbe),
            codex_executable: PathBuf::from("codex"),
            claude_executable: PathBuf::from("claude"),
            configuration_home_override: None,
            target_runtime: ReconciliationTargetRuntime::new(
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(())),
                Arc::new(Mutex::new(())),
            ),
        };
        let peer = ReconciliationService::from_runtime(Arc::clone(&state), runtime.clone());
        let gate = peer.lock_target_mutation(Target::Codex).await;
        let service = Arc::new(ProviderSynchronizationService::from_runtime(
            Arc::clone(&state),
            runtime,
        ));
        let mut synchronization = tokio::spawn({
            let service = Arc::clone(&service);
            async move {
                service
                    .apply_raw(
                        uuid::Uuid::from_u128(0x902),
                        1,
                        serde_json::json!({
                            "kind": "synchronize-universal-provider",
                            "providerId": provider_id,
                            "providerRevision": provider_revision
                        }),
                        None,
                    )
                    .await
            }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut synchronization)
                .await
                .is_err(),
            "synchronization bypassed the shared Target mutation gate"
        );
        assert!(
            state
                .target_view_for(Target::Codex)
                .await
                .unwrap()
                .providers
                .is_empty()
        );
        drop(gate);
        let synchronized = tokio::time::timeout(std::time::Duration::from_secs(1), synchronization)
            .await
            .expect("synchronization did not resume after the shared gate opened")
            .unwrap()
            .result
            .unwrap();

        assert_eq!(synchronized.target_views.len(), 2);
        assert!(
            synchronized.outcome.view.providers[0]
                .targets
                .iter()
                .all(|target| target.synchronization
                    == crate::control::protocol::UniversalSynchronizationState::Current)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn drift_on_one_target_blocks_the_whole_synchronization() {
        let root = std::env::temp_dir().join(format!(
            "muxvia-provider-synchronization-drift-{}",
            uuid::Uuid::new_v4()
        ));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let state = Arc::new(StateStore::open(&home).await.unwrap());
        let created = state
            .apply_universal_provider_action(
                uuid::Uuid::from_u128(0x911),
                0,
                serde_json::json!({
                    "kind": "create-universal-provider",
                    "name": "Blocked Source",
                    "baseUrl": "https://blocked.example/v1",
                    "credential": { "kind": "replace", "value": "BLOCKED_SECRET_77312" },
                    "presetKey": null,
                    "targets": [
                        { "target": "codex", "enabled": true, "model": "codex-model", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                        { "target": "claude", "enabled": true, "model": "claude-model", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                    ]
                }),
            )
            .await
            .unwrap();
        state
            .mark_configuration_drift_for(crate::control::protocol::Target::Claude)
            .await
            .unwrap();
        let runtime = ReconciliationRuntime {
            home: home.clone(),
            codex_probe: Arc::new(TestedCodexProbe),
            claude_probe: Arc::new(TestedClaudeProbe),
            codex_executable: PathBuf::from("codex"),
            claude_executable: PathBuf::from("claude"),
            configuration_home_override: None,
            target_runtime: ReconciliationTargetRuntime::new(
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(())),
                Arc::new(Mutex::new(())),
            ),
        };
        let service = ProviderSynchronizationService::from_runtime(Arc::clone(&state), runtime);

        let failure = service
            .apply_raw(
                uuid::Uuid::from_u128(0x912),
                1,
                serde_json::json!({
                    "kind": "synchronize-universal-provider",
                    "providerId": created.view.providers[0].id,
                    "providerRevision": 1
                }),
                None,
            )
            .await
            .result
            .unwrap_err();

        assert_eq!(failure.problem.code, "configuration-drift");
        assert!(
            failure.authoritative_view.providers[0]
                .targets
                .iter()
                .all(|target| target.generated_provider_id.is_none())
        );
        for target in [
            crate::control::protocol::Target::Codex,
            crate::control::protocol::Target::Claude,
        ] {
            assert!(
                state
                    .target_view_for(target)
                    .await
                    .unwrap()
                    .providers
                    .is_empty()
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn synchronization_requires_exact_unknown_compatibility_acknowledgement() {
        let root = std::env::temp_dir().join(format!(
            "muxvia-provider-synchronization-ack-{}",
            uuid::Uuid::new_v4()
        ));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let state = Arc::new(StateStore::open(&home).await.unwrap());
        let created = state
            .apply_universal_provider_action(
                uuid::Uuid::from_u128(0x921),
                0,
                serde_json::json!({
                    "kind": "create-universal-provider",
                    "name": "Unknown Compatibility Source",
                    "baseUrl": "https://unknown.example/v1",
                    "credential": { "kind": "remove" },
                    "presetKey": null,
                    "targets": [
                        { "target": "codex", "enabled": true, "model": "codex-model", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                        { "target": "claude", "enabled": false, "model": "claude-model", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                    ]
                }),
            )
            .await
            .unwrap();
        let provider = &created.view.providers[0];
        let runtime = ReconciliationRuntime {
            home: home.clone(),
            codex_probe: Arc::new(UnknownCodexProbe),
            claude_probe: Arc::new(TestedClaudeProbe),
            codex_executable: PathBuf::from("codex"),
            claude_executable: PathBuf::from("claude"),
            configuration_home_override: None,
            target_runtime: ReconciliationTargetRuntime::new(
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(())),
                Arc::new(Mutex::new(())),
            ),
        };
        let service = ProviderSynchronizationService::from_runtime(Arc::clone(&state), runtime);
        let action = serde_json::json!({
            "kind": "synchronize-universal-provider",
            "providerId": provider.id,
            "providerRevision": provider.provider_revision
        });

        let failure = service
            .apply_raw(uuid::Uuid::from_u128(0x922), 1, action.clone(), None)
            .await
            .result
            .unwrap_err();
        assert_eq!(
            failure.problem.code,
            "compatibility-acknowledgement-required"
        );
        assert!(
            failure.authoritative_view.providers[0].targets[0]
                .generated_provider_id
                .is_none()
        );

        state
            .acknowledge_compatibility(crate::control::protocol::Target::Codex, "codex-next")
            .await
            .unwrap();
        let unrelated_gate = service
            .reconciliation
            .lock_target_mutation(Target::Claude)
            .await;
        let synchronized = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            service.apply_raw(uuid::Uuid::from_u128(0x922), 1, action, None),
        )
        .await
        .expect("Codex-only synchronization waited for the unrelated Claude gate")
        .result
        .unwrap();
        drop(unrelated_gate);
        assert_eq!(synchronized.target_views.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn incompatible_or_recovery_required_target_blocks_the_whole_synchronization() {
        for (index, expected_code) in ["incompatible-target-cli", "recovery-required"]
            .into_iter()
            .enumerate()
        {
            let root = std::env::temp_dir().join(format!(
                "muxvia-provider-synchronization-blocker-{index}-{}",
                uuid::Uuid::new_v4()
            ));
            let user_home = root.join("home");
            fs::create_dir_all(&user_home).unwrap();
            let home = MuxviaHome::from_user_home(&user_home);
            let state = Arc::new(StateStore::open(&home).await.unwrap());
            let created = state
                .apply_universal_provider_action(
                    uuid::Uuid::from_u128(0x931 + index as u128),
                    0,
                    serde_json::json!({
                        "kind": "create-universal-provider",
                        "name": "Blocked Whole Source",
                        "baseUrl": "https://whole-block.example/v1",
                        "credential": { "kind": "remove" },
                        "presetKey": null,
                        "targets": [
                            { "target": "codex", "enabled": true, "model": "codex-model", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                            { "target": "claude", "enabled": true, "model": "claude-model", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                        ]
                    }),
                )
                .await
                .unwrap();
            if expected_code == "recovery-required" {
                let database = tokio_rusqlite::Connection::open(home.database_path())
                    .await
                    .unwrap();
                database
                    .call(|connection| {
                        connection.execute(
                            "UPDATE target_route_state
                             SET recovery_state = 'recovery-required'
                             WHERE target = 'claude'",
                            [],
                        )?;
                        Ok::<_, tokio_rusqlite::rusqlite::Error>(())
                    })
                    .await
                    .unwrap();
            }
            let codex_probe: Arc<dyn CodexProbe> = if expected_code == "incompatible-target-cli" {
                Arc::new(IncompatibleCodexProbe)
            } else {
                Arc::new(TestedCodexProbe)
            };
            let runtime = ReconciliationRuntime {
                home: home.clone(),
                codex_probe,
                claude_probe: Arc::new(TestedClaudeProbe),
                codex_executable: PathBuf::from("codex"),
                claude_executable: PathBuf::from("claude"),
                configuration_home_override: None,
                target_runtime: ReconciliationTargetRuntime::new(
                    Arc::new(Mutex::new(None)),
                    Arc::new(Mutex::new(None)),
                    Arc::new(Mutex::new(())),
                    Arc::new(Mutex::new(())),
                ),
            };
            let service = ProviderSynchronizationService::from_runtime(Arc::clone(&state), runtime);

            let attempt = service
                .apply_raw(
                    uuid::Uuid::from_u128(0x941 + index as u128),
                    1,
                    serde_json::json!({
                        "kind": "synchronize-universal-provider",
                        "providerId": created.view.providers[0].id,
                        "providerRevision": 1
                    }),
                    None,
                )
                .await;

            if expected_code == "incompatible-target-cli" {
                assert_eq!(
                    attempt.eligibility_publication.as_ref().unwrap().target,
                    Target::Codex
                );
            } else {
                assert!(attempt.eligibility_publication.is_none());
            }
            let failure = attempt.result.unwrap_err();
            assert_eq!(failure.problem.code, expected_code);
            assert_eq!(
                state.universal_provider_catalog().await.unwrap(),
                created.view
            );
            for target in [Target::Codex, Target::Claude] {
                assert!(
                    state
                        .target_view_for(target)
                        .await
                        .unwrap()
                        .providers
                        .is_empty()
                );
            }
            let _ = fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn managed_shadow_blocks_synchronization_without_changing_runtime_projection() {
        const UNIVERSAL_SECRET: &str = "SHADOWED_UNIVERSAL_SECRET_73519";
        let root = std::env::temp_dir().join(format!(
            "muxvia-provider-synchronization-shadow-{}",
            uuid::Uuid::new_v4()
        ));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let state = Arc::new(StateStore::open(&home).await.unwrap());
        let target_provider = state
            .apply_provider_action_for(
                Target::Codex,
                uuid::Uuid::from_u128(0x951),
                0,
                serde_json::json!({
                    "kind": "create-provider",
                    "name": "Managed Codex",
                    "baseUrl": "https://managed.example/v1",
                    "model": "managed-model",
                    "credential": { "kind": "replace", "value": "MANAGED_SECRET_83410" },
                    "authentication": ProviderAuthentication::OpenaiBearer,
                    "presetKey": null
                }),
            )
            .await
            .unwrap();
        let activation = ActivationService::new(
            Arc::clone(&state),
            home.clone(),
            Arc::new(TestedCodexProbe),
            PathBuf::from("codex"),
            Arc::new(NoopUpstream),
        );
        activation
            .apply_raw_for(
                Target::Codex,
                uuid::Uuid::from_u128(0x952),
                1,
                serde_json::json!({
                    "kind": "activate-provider",
                    "providerId": target_provider.view.providers[0].id,
                    "mode": "direct"
                }),
            )
            .await
            .unwrap();
        let config_path = home.user_home().join(".codex/config.toml");
        let managed_config = fs::read_to_string(&config_path).unwrap();
        fs::write(
            &config_path,
            format!("profile = \"operator-profile\"\n{managed_config}"),
        )
        .unwrap();
        let created = state
            .apply_universal_provider_action(
                uuid::Uuid::from_u128(0x953),
                0,
                serde_json::json!({
                    "kind": "create-universal-provider",
                    "name": "Shadowed Source",
                    "baseUrl": "https://shadowed.example/v1",
                    "credential": { "kind": "replace", "value": UNIVERSAL_SECRET },
                    "presetKey": null,
                    "targets": [
                        { "target": "codex", "enabled": true, "model": "generated-model", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                        { "target": "claude", "enabled": false, "model": "claude-model", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                    ]
                }),
            )
            .await
            .unwrap();
        let target_before = state.target_view_for(Target::Codex).await.unwrap();
        let runtime = activation.reconciliation_runtime();
        let service = ProviderSynchronizationService::from_runtime(Arc::clone(&state), runtime);

        let attempt = service
            .apply_raw(
                uuid::Uuid::from_u128(0x954),
                1,
                serde_json::json!({
                    "kind": "synchronize-universal-provider",
                    "providerId": created.view.providers[0].id,
                    "providerRevision": 1
                }),
                None,
            )
            .await;

        assert_eq!(
            attempt.eligibility_publication.as_ref().unwrap().target,
            Target::Codex
        );
        let failure = attempt.result.unwrap_err();
        assert!(!format!("{failure:?}").contains(UNIVERSAL_SECRET));
        assert_eq!(failure.problem.code, "shadowing-configuration");
        assert_eq!(failure.problem.source.as_deref(), Some("codex-profile"));
        assert_eq!(
            state.universal_provider_catalog().await.unwrap(),
            created.view
        );
        let target_after = state.target_view_for(Target::Codex).await.unwrap();
        assert_eq!(target_after.providers, target_before.providers);
        assert_eq!(
            target_after.current_provider_id,
            target_before.current_provider_id
        );
        assert_eq!(
            target_after.serving_provider_id,
            target_before.serving_provider_id
        );
        assert_eq!(target_after.takeover, target_before.takeover);
        assert_eq!(
            target_after.activated_snapshot,
            target_before.activated_snapshot
        );
        assert_eq!(target_after.recovery, target_before.recovery);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn synchronization_preserves_takeover_listener_and_held_request() {
        let root = std::path::PathBuf::from("/tmp").join(format!(
            "mx-universal-held-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let state = Arc::new(StateStore::open(&home).await.unwrap());
        let target_provider = state
            .apply_provider_action_for(
                Target::Codex,
                uuid::Uuid::from_u128(0x961),
                0,
                serde_json::json!({
                    "kind": "create-provider",
                    "name": "Pinned Target Provider",
                    "baseUrl": "https://pinned.example/v1",
                    "model": "pinned-model",
                    "credential": { "kind": "replace", "value": "PINNED_SECRET_14822" },
                    "authentication": ProviderAuthentication::OpenaiBearer,
                    "presetKey": null
                }),
            )
            .await
            .unwrap();
        let upstream = Arc::new(HeldUpstream::new());
        let activation = Arc::new(ActivationService::new(
            Arc::clone(&state),
            home.clone(),
            Arc::new(TestedCodexProbe),
            PathBuf::from("codex"),
            upstream.clone(),
        ));
        activation
            .apply_raw_for(
                Target::Codex,
                uuid::Uuid::from_u128(0x962),
                1,
                serde_json::json!({
                    "kind": "activate-provider",
                    "providerId": target_provider.view.providers[0].id,
                    "mode": "takeover"
                }),
            )
            .await
            .unwrap();
        let endpoint = activation.model_endpoint_for(Target::Codex).await.unwrap();
        let routing_credential = state
            .routing_credential_for(Target::Codex)
            .await
            .unwrap()
            .unwrap();
        let request_started = upstream.started.notified();
        let held_request = tokio::spawn({
            let routing_credential = routing_credential.expose_secret().to_owned();
            async move {
                reqwest::Client::new()
                    .post(format!("http://{endpoint}/v1/responses"))
                    .header("X-Muxvia-Routing-Credential", routing_credential)
                    .body("{}")
                    .send()
                    .await
                    .unwrap()
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), request_started)
            .await
            .expect("held request did not enter the upstream");
        let created = state
            .apply_universal_provider_action(
                uuid::Uuid::from_u128(0x963),
                0,
                serde_json::json!({
                    "kind": "create-universal-provider",
                    "name": "Declaration Only Source",
                    "baseUrl": "https://declaration-only.example/v1",
                    "credential": { "kind": "replace", "value": "GENERATED_SECRET_32119" },
                    "presetKey": null,
                    "targets": [
                        { "target": "codex", "enabled": true, "model": "generated-model", "authentication": "openai-bearer", "routingRequirement": "takeover-required" },
                        { "target": "claude", "enabled": false, "model": "claude-model", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                    ]
                }),
            )
            .await
            .unwrap();
        let before = state.target_view_for(Target::Codex).await.unwrap();
        let service = ProviderSynchronizationService::from_runtime(
            Arc::clone(&state),
            activation.reconciliation_runtime(),
        );

        let synchronized = service
            .apply_raw(
                uuid::Uuid::from_u128(0x964),
                1,
                serde_json::json!({
                    "kind": "synchronize-universal-provider",
                    "providerId": created.view.providers[0].id,
                    "providerRevision": 1
                }),
                None,
            )
            .await
            .result
            .unwrap();

        assert_eq!(synchronized.target_views.len(), 1);
        let after = state.target_view_for(Target::Codex).await.unwrap();
        assert_eq!(after.current_provider_id, before.current_provider_id);
        assert_eq!(after.serving_provider_id, before.serving_provider_id);
        assert_eq!(after.activated_snapshot, before.activated_snapshot);
        assert_eq!(after.takeover, before.takeover);
        assert_eq!(
            activation.model_endpoint_for(Target::Codex).await,
            Some(endpoint)
        );
        upstream.release.notify_one();
        assert_eq!(held_request.await.unwrap().status(), StatusCode::OK);
        activation.shutdown_models().await.unwrap();
        let _ = fs::remove_dir_all(root);
    }
}
