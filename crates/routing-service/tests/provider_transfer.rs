use std::{path::PathBuf, sync::Arc};

use muxvia_routing::{
    control::protocol::{
        ProviderImportCandidateView, ProviderImportChoice, ProviderImportProduct,
        ProviderImportResolution, ProviderImportSource, ProviderImportSourceTarget, Target,
    },
    home::MuxviaHome,
    service::provider_transfer::{ProviderTransferError, ProviderTransferService},
    state::StateStore,
};
use tempfile::TempDir;
use uuid::Uuid;

async fn fixture() -> (TempDir, PathBuf, Arc<StateStore>, ProviderTransferService) {
    let root = tempfile::tempdir().unwrap();
    let user_home = root.path().join("home");
    std::fs::create_dir(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let transfer = ProviderTransferService::new(Arc::clone(&store), home);
    (root, user_home, store, transfer)
}

#[tokio::test]
async fn pasted_ccswitch_preview_finds_only_an_exact_normalized_match_without_mutation_or_disclosure()
 {
    let (_root, _user_home, store, transfer) = fixture().await;
    let before = store.target_view_for(Target::Codex).await.unwrap();
    let saved = store
        .apply_provider_action_for(
            Target::Codex,
            Uuid::new_v4(),
            before.management_revision,
            serde_json::json!({
                "kind": "create-provider",
                "name": "Different display name",
                "baseUrl": "https://relay.example/v1/",
                "model": "gpt-5",
                "credential": {
                    "kind": "replace",
                    "value": "ccswitch-preview-secret-must-not-escape"
                },
                "authentication": "openai-bearer",
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    let existing_id = saved.view.providers[0].id;
    let revision_before_preview = saved.view.management_revision;

    let preview = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::CcSwitch {
                payload: "ccswitch://v1/import?resource=provider&app=codex&name=Relay&endpoint=https%3A%2F%2Frelay.example%2Fv1&apiKey=ccswitch-preview-secret-must-not-escape&model=gpt-5".to_owned(),
            },
        )
        .await
        .unwrap();

    assert_eq!(preview.source.product, ProviderImportProduct::CcSwitch);
    assert_eq!(preview.source.target, ProviderImportSourceTarget::Codex);
    assert_eq!(preview.candidates.len(), 1);
    let ProviderImportCandidateView::TargetProvider {
        name,
        base_url,
        imported_current,
        exact_matches,
        ..
    } = &preview.candidates[0]
    else {
        panic!("expected Target Provider candidate")
    };
    assert_eq!(name, "Relay");
    assert_eq!(base_url, "https://relay.example/v1");
    assert!(!imported_current);
    assert_eq!(
        exact_matches
            .iter()
            .map(|matched| matched.provider_id)
            .collect::<Vec<_>>(),
        vec![existing_id]
    );
    assert!(
        !serde_json::to_string(&preview)
            .unwrap()
            .contains("ccswitch-preview-secret-must-not-escape")
    );
    assert!(!format!("{preview:?}").contains("ccswitch-preview-secret-must-not-escape"));

    let after = store.target_view_for(Target::Codex).await.unwrap();
    assert_eq!(after.management_revision, revision_before_preview);
    assert_eq!(after.providers.len(), 1);
}

#[tokio::test]
async fn exact_existing_confirmation_is_one_shot_identity_safe_and_does_not_mutate_the_match() {
    let (_root, _user_home, store, transfer) = fixture().await;
    let before = store.target_view_for(Target::Codex).await.unwrap();
    let saved = store
        .apply_provider_action_for(
            Target::Codex,
            Uuid::new_v4(),
            before.management_revision,
            serde_json::json!({
                "kind": "create-provider",
                "name": "Existing Identity",
                "baseUrl": "https://exact.example/v1",
                "model": "gpt-exact",
                "credential": { "kind": "replace", "value": "exact-confirm-secret" },
                "authentication": "openai-bearer",
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    let existing = saved.view.providers[0].clone();
    let preview = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::CcSwitch {
                payload: "ccswitch://v1/import?resource=provider&app=codex&name=Different+Imported+Name&endpoint=https%3A%2F%2Fexact.example%2Fv1%2F&apiKey=exact-confirm-secret&model=gpt-exact".to_owned(),
            },
        )
        .await
        .unwrap();
    let candidate_id = match preview.candidates[0] {
        ProviderImportCandidateView::TargetProvider { candidate_id, .. } => candidate_id,
        _ => panic!("expected target candidate"),
    };

    let outcome = transfer
        .confirm(
            Target::Codex,
            preview.preview_token,
            vec![ProviderImportChoice {
                candidate_id,
                resolution: ProviderImportResolution::UseExisting {
                    provider_id: existing.id,
                },
            }],
        )
        .await
        .unwrap();
    assert_eq!(outcome.records.len(), 1);
    let after = store.target_view_for(Target::Codex).await.unwrap();
    assert_eq!(after.management_revision, saved.view.management_revision);
    assert_eq!(after.providers[0].id, existing.id);
    assert_eq!(after.providers[0].name, "Existing Identity");

    let replay = transfer
        .confirm(
            Target::Codex,
            preview.preview_token,
            vec![ProviderImportChoice {
                candidate_id,
                resolution: ProviderImportResolution::UseExisting {
                    provider_id: existing.id,
                },
            }],
        )
        .await
        .unwrap_err();
    assert!(matches!(replay, ProviderTransferError::PreviewExpired));
}

#[tokio::test]
async fn live_target_preview_marks_a_distinct_configuration_as_imported_current_without_rewrite() {
    let (_root, user_home, store, transfer) = fixture().await;
    let configuration_home = user_home.join(".codex");
    std::fs::create_dir(&configuration_home).unwrap();
    let path = configuration_home.join("config.toml");
    let live = r#"model = "gpt-live"
model_provider = "operator-live"

[model_providers.operator-live]
name = "Operator Live"
base_url = "https://live.example/v1/"
wire_api = "responses"
http_headers = { Authorization = "Bearer live-import-secret-must-not-escape" }
supports_websockets = false
"#;
    std::fs::write(&path, live).unwrap();

    let before = store.target_view_for(Target::Codex).await.unwrap();
    let preview = transfer
        .preview(Target::Codex, ProviderImportSource::LiveTarget)
        .await
        .unwrap();

    assert_eq!(preview.source.product, ProviderImportProduct::TargetCli);
    assert_eq!(preview.source.target, ProviderImportSourceTarget::Codex);
    let ProviderImportCandidateView::TargetProvider {
        name,
        base_url,
        model,
        imported_current,
        exact_matches,
        ..
    } = &preview.candidates[0]
    else {
        panic!("expected live Target Provider candidate")
    };
    assert_eq!(name, "Operator Live");
    assert_eq!(base_url, "https://live.example/v1");
    assert_eq!(model, "gpt-live");
    assert!(*imported_current);
    assert!(exact_matches.is_empty());
    assert!(
        !serde_json::to_string(&preview)
            .unwrap()
            .contains("live-import-secret-must-not-escape")
    );

    assert_eq!(std::fs::read_to_string(&path).unwrap(), live);
    let after = store.target_view_for(Target::Codex).await.unwrap();
    assert_eq!(after.management_revision, before.management_revision);
    assert_eq!(after.current_provider_id, before.current_provider_id);
    assert!(after.providers.is_empty());
}

#[tokio::test]
async fn live_claude_preview_preserves_api_key_authentication_without_rewrite() {
    let (_root, user_home, store, transfer) = fixture().await;
    let configuration_home = user_home.join(".claude");
    std::fs::create_dir(&configuration_home).unwrap();
    let path = configuration_home.join("settings.json");
    let live = serde_json::json!({
        "env": {
            "ANTHROPIC_BASE_URL": "https://claude-live.example/v1/",
            "ANTHROPIC_API_KEY": "claude-live-import-secret-must-not-escape",
            "ANTHROPIC_MODEL": "claude-sonnet-live"
        },
        "permissions": { "allow": ["Read"] }
    });
    let bytes = serde_json::to_vec_pretty(&live).unwrap();
    std::fs::write(&path, &bytes).unwrap();

    let before = store.target_view_for(Target::Claude).await.unwrap();
    let preview = transfer
        .preview(Target::Claude, ProviderImportSource::LiveTarget)
        .await
        .unwrap();
    let ProviderImportCandidateView::TargetProvider {
        authentication,
        imported_current,
        exact_matches,
        ..
    } = &preview.candidates[0]
    else {
        panic!("expected live Claude Target Provider candidate")
    };
    assert_eq!(
        *authentication,
        muxvia_routing::control::protocol::ProviderAuthentication::AnthropicApiKey
    );
    assert!(*imported_current);
    assert!(exact_matches.is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let after = store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(after.management_revision, before.management_revision);
    assert_eq!(after.current_provider_id, before.current_provider_id);
}

#[tokio::test]
async fn a_live_muxvia_routing_credential_is_rejected_without_echo_or_mutation() {
    const ROUTING_SECRET: &str = "muxvia-routing-secret-must-not-be-imported";
    let (_root, user_home, store, transfer) = fixture().await;
    let database =
        tokio_rusqlite::Connection::open(MuxviaHome::from_user_home(&user_home).database_path())
            .await
            .unwrap();
    database
        .call(
            |connection| -> Result<(), tokio_rusqlite::rusqlite::Error> {
                connection.execute(
                    "UPDATE target_route_state SET routing_credential = ?1 WHERE target = 'claude'",
                    [ROUTING_SECRET],
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let configuration_home = user_home.join(".claude");
    std::fs::create_dir(&configuration_home).unwrap();
    let path = configuration_home.join("settings.json");
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "env": {
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:4567",
            "ANTHROPIC_AUTH_TOKEN": ROUTING_SECRET,
            "ANTHROPIC_MODEL": "routed-model"
        }
    }))
    .unwrap();
    std::fs::write(&path, &bytes).unwrap();

    let error = transfer
        .preview(Target::Claude, ProviderImportSource::LiveTarget)
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderTransferError::SecretRejected));
    assert!(!format!("{error:?}").contains(ROUTING_SECRET));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(
        store
            .target_view_for(Target::Claude)
            .await
            .unwrap()
            .providers
            .is_empty()
    );
}

fn muxvia_export_value() -> serde_json::Value {
    serde_json::json!({
        "format": "muxvia-provider-configuration",
        "version": 1,
        "universalProviders": [{
            "sourceId": "40000000-0000-4000-8000-000000000001",
            "position": 0,
            "name": "Shared Relay",
            "baseUrl": "https://shared.example/v1/",
            "targets": [{
                "target": "codex",
                "enabled": true,
                "model": "gpt-shared",
                "authentication": "openai-bearer",
                "routingRequirement": "direct-compatible"
            }, {
                "target": "claude",
                "enabled": false,
                "model": "",
                "authentication": "anthropic-bearer",
                "routingRequirement": "direct-compatible"
            }]
        }],
        "targetProviders": [{
            "sourceId": "50000000-0000-4000-8000-000000000001",
            "target": "codex",
            "position": 0,
            "name": "Shared Relay",
            "baseUrl": "https://shared.example/v1/",
            "model": "gpt-shared",
            "protocol": "openai-responses",
            "authentication": "openai-bearer",
            "routingRequirement": "direct-compatible",
            "universalProviderSourceId": "40000000-0000-4000-8000-000000000001"
        }, {
            "sourceId": "50000000-0000-4000-8000-000000000002",
            "target": "claude",
            "position": 0,
            "name": "Claude Relay",
            "baseUrl": "https://claude.example/v1",
            "model": "claude-exported",
            "protocol": "anthropic-messages",
            "authentication": "anthropic-bearer",
            "routingRequirement": "direct-compatible",
            "universalProviderSourceId": null
        }],
        "failoverDrafts": [{
            "target": "codex",
            "providerSourceIds": ["50000000-0000-4000-8000-000000000001"]
        }, {
            "target": "claude",
            "providerSourceIds": ["50000000-0000-4000-8000-000000000002"]
        }]
    })
}

#[tokio::test]
async fn muxvia_export_preview_normalizes_redacted_universal_and_target_declarations_without_mutation()
 {
    let (_root, _user_home, store, transfer) = fixture().await;
    let before_codex = store.target_view_for(Target::Codex).await.unwrap();
    let before_claude = store.target_view_for(Target::Claude).await.unwrap();

    let preview = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::MuxviaExport {
                payload: serde_json::to_string(&muxvia_export_value()).unwrap(),
            },
        )
        .await
        .unwrap();

    assert_eq!(preview.source.product, ProviderImportProduct::Muxvia);
    assert_eq!(preview.source.target, ProviderImportSourceTarget::Universal);
    assert_eq!(preview.candidates.len(), 2);
    assert!(preview.candidates.iter().all(|candidate| match candidate {
        ProviderImportCandidateView::TargetProvider { credential, .. }
        | ProviderImportCandidateView::UniversalProvider { credential, .. } => {
            *credential == muxvia_routing::control::protocol::CredentialPresence::Missing
        }
    }));
    assert!(preview.candidates.iter().any(|candidate| matches!(
        candidate,
        ProviderImportCandidateView::UniversalProvider { base_url, .. }
            if base_url == "https://shared.example/v1"
    )));

    let after_codex = store.target_view_for(Target::Codex).await.unwrap();
    let after_claude = store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(
        after_codex.management_revision,
        before_codex.management_revision
    );
    assert_eq!(
        after_claude.management_revision,
        before_claude.management_revision
    );
    assert!(after_codex.providers.is_empty());
    assert!(after_claude.providers.is_empty());
}

#[tokio::test]
async fn corrupt_oversized_duplicate_and_hostile_previews_fail_atomically_without_secret_echo() {
    let (_root, _user_home, store, transfer) = fixture().await;
    let before_codex = store.target_view_for(Target::Codex).await.unwrap();
    let before_catalog = store.universal_provider_catalog().await.unwrap();

    let secret = "corrupt-import-secret-must-not-escape";
    let error = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::MuxviaExport {
                payload: format!(r#"{{"credential":"{secret}""#),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderTransferError::InvalidImport));
    assert!(!format!("{error:?}").contains(secret));

    let error = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::CcSwitch {
                payload: format!(
                    "ccswitch://v1/import?resource=provider&app=codex&name={}",
                    "x".repeat(524_289)
                ),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderTransferError::ImportTooLarge));

    let error = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::CcSwitch {
                payload: "ccswitch://v1/import?resource=provider&app=codex&name=Relay&apiKey=first&apiKey=second".to_owned(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderTransferError::HostileImport));

    let mut duplicate_ids = muxvia_export_value();
    duplicate_ids["targetProviders"][1]["sourceId"] =
        duplicate_ids["targetProviders"][0]["sourceId"].clone();
    let error = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::MuxviaExport {
                payload: serde_json::to_string(&duplicate_ids).unwrap(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderTransferError::HostileImport));

    let mut mismatched_generated_owner = muxvia_export_value();
    mismatched_generated_owner["targetProviders"][0]["model"] = "tampered".into();
    let error = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::MuxviaExport {
                payload: serde_json::to_string(&mismatched_generated_owner).unwrap(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderTransferError::HostileImport));

    let after_codex = store.target_view_for(Target::Codex).await.unwrap();
    let after_catalog = store.universal_provider_catalog().await.unwrap();
    assert_eq!(
        after_codex.management_revision,
        before_codex.management_revision
    );
    assert_eq!(after_codex.providers, before_codex.providers);
    assert_eq!(after_catalog.revision, before_catalog.revision);
    assert_eq!(after_catalog.providers, before_catalog.providers);
}

#[tokio::test]
async fn equal_names_with_distinct_normalized_configurations_coexist_in_preview_but_duplicate_configurations_do_not()
 {
    let (_root, _user_home, _store, transfer) = fixture().await;
    let mut equal_names = muxvia_export_value();
    equal_names["targetProviders"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "sourceId": "50000000-0000-4000-8000-000000000003",
            "target": "claude",
            "position": 1,
            "name": "Claude Relay",
            "baseUrl": "https://distinct.example/v1/",
            "model": "claude-exported",
            "protocol": "anthropic-messages",
            "authentication": "anthropic-bearer",
            "routingRequirement": "direct-compatible",
            "universalProviderSourceId": null
        }));
    let preview = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::MuxviaExport {
                payload: serde_json::to_string(&equal_names).unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(preview.candidates.len(), 3);

    equal_names["targetProviders"][2]["baseUrl"] = "https://claude.example/v1/".into();
    let error = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::MuxviaExport {
                payload: serde_json::to_string(&equal_names).unwrap(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderTransferError::HostileImport));
}

#[tokio::test]
async fn export_is_an_atomic_always_redacted_snapshot_that_round_trips_through_preview() {
    const TARGET_SECRET: &str = "target-export-secret-must-not-escape";
    const UNIVERSAL_SECRET: &str = "universal-export-secret-must-not-escape";
    let (_root, user_home, store, transfer) = fixture().await;

    let codex = store.target_view_for(Target::Codex).await.unwrap();
    let codex = store
        .apply_provider_action_for(
            Target::Codex,
            Uuid::new_v4(),
            codex.management_revision,
            serde_json::json!({
                "kind": "create-provider",
                "name": "Codex Local",
                "baseUrl": "https://codex-local.example/v1",
                "model": "gpt-local",
                "credential": { "kind": "replace", "value": TARGET_SECRET },
                "authentication": "openai-bearer",
                "presetKey": null
            }),
        )
        .await
        .unwrap();
    let codex_local = codex.view.providers[0].clone();

    let universal = store
        .apply_universal_provider_action(
            Uuid::new_v4(),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Shared Export",
                "baseUrl": "https://shared-export.example/v1",
                "credential": { "kind": "replace", "value": UNIVERSAL_SECRET },
                "presetKey": null,
                "targets": [{
                    "target": "codex",
                    "enabled": true,
                    "model": "gpt-shared-export",
                    "authentication": "openai-bearer",
                    "routingRequirement": "direct-compatible"
                }, {
                    "target": "claude",
                    "enabled": true,
                    "model": "claude-shared-export",
                    "authentication": "anthropic-bearer",
                    "routingRequirement": "takeover-required"
                }]
            }),
        )
        .await
        .unwrap();
    let universal_id = universal.view.providers[0].id;
    let synchronized = store
        .synchronize_universal_provider_action(Uuid::new_v4(), 1, universal_id, 1)
        .await
        .unwrap();
    let universal_view = &synchronized.outcome.view.providers[0];
    let generated_codex = universal_view.targets[0].generated_provider_id.unwrap();
    let generated_claude = universal_view.targets[1].generated_provider_id.unwrap();

    let database =
        tokio_rusqlite::Connection::open(MuxviaHome::from_user_home(&user_home).database_path())
            .await
            .unwrap();
    database
        .call(
            move |connection| -> Result<(), tokio_rusqlite::rusqlite::Error> {
                connection.execute(
                    "INSERT INTO failover_draft_members
                 (target, position, provider_id, provider_revision)
                 VALUES ('codex', 0, ?1, ?2)",
                    tokio_rusqlite::rusqlite::params![
                        codex_local.id.to_string(),
                        codex_local.provider_revision
                    ],
                )?;
                connection.execute(
                    "INSERT INTO failover_draft_members
                 (target, position, provider_id, provider_revision)
                 VALUES ('codex', 1, ?1, 1)",
                    [generated_codex.to_string()],
                )?;
                connection.execute(
                    "INSERT INTO failover_draft_members
                 (target, position, provider_id, provider_revision)
                 VALUES ('claude', 0, ?1, 1)",
                    [generated_claude.to_string()],
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();

    let export = transfer.export().await.unwrap();
    let serialized = serde_json::to_string(&export).unwrap();
    assert!(!serialized.contains(TARGET_SECRET));
    assert!(!serialized.contains(UNIVERSAL_SECRET));
    for forbidden in [
        "credential",
        "token",
        "subscription",
        "currentProvider",
        "servingProvider",
        "activatedSnapshot",
        "recovery",
    ] {
        assert!(
            !serialized
                .to_ascii_lowercase()
                .contains(&forbidden.to_ascii_lowercase())
        );
    }
    assert_eq!(export.universal_providers.len(), 1);
    assert_eq!(export.target_providers.len(), 3);
    assert_eq!(export.failover_drafts[0].provider_source_ids.len(), 2);
    assert_eq!(export.failover_drafts[1].provider_source_ids.len(), 1);

    let preview = transfer
        .preview(
            Target::Codex,
            ProviderImportSource::MuxviaExport {
                payload: serialized,
            },
        )
        .await
        .unwrap();
    assert_eq!(preview.candidates.len(), 2);
    assert!(preview.candidates.iter().all(|candidate| match candidate {
        ProviderImportCandidateView::TargetProvider { exact_matches, .. }
        | ProviderImportCandidateView::UniversalProvider { exact_matches, .. } => {
            exact_matches.is_empty()
        }
    }));
}
