use std::{fs, path::PathBuf};

use muxvia_routing::{
    codex::CodexConfigCodec,
    control::protocol::{
        ActionStatus, ClientFrame, ControlOperation, CredentialEdit, CredentialPresence, Target,
        TargetAction,
    },
    domain::provider::normalize_provider_base_url,
    home::MuxviaHome,
    state::{RecoveryIntent, RecoveryPayload, StateStore},
};
use uuid::Uuid;

struct StoreFixture {
    root: PathBuf,
    home: MuxviaHome,
    store: std::sync::Arc<StateStore>,
}

impl StoreFixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("muxvia-state-test-{}", Uuid::new_v4()));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = std::sync::Arc::new(StateStore::open(&home).await.unwrap());
        Self { root, home, store }
    }
}

impl Drop for StoreFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixed_uuid(last_byte: u8) -> Uuid {
    let mut bytes = [0; 16];
    bytes[15] = last_byte;
    Uuid::from_bytes(bytes)
}

fn raw_save_provider(
    name: &str,
    base_url: &str,
    model: &str,
    credential: &str,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "save-provider",
        "name": name,
        "baseUrl": base_url,
        "model": model,
        "credential": credential
    })
}

#[tokio::test]
async fn new_database_target_view_matches_the_canonical_protocol_fixture() {
    let fixture = StoreFixture::new().await;
    let view = fixture.store.target_view().await.unwrap();
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/fixtures/initial-target-view.json");
    let mut expected: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture_path).unwrap()).unwrap();
    expected["service"]["epoch"] = serde_json::json!(view.service.epoch);

    assert_eq!(serde_json::to_value(view).unwrap(), expected);
}

#[tokio::test]
async fn fresh_schema_v4_reopens_with_codex_and_claude_route_rows() {
    let root = std::env::temp_dir().join(format!("muxvia-v4-reopen-{}", Uuid::new_v4()));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let first = StateStore::open(&home).await.unwrap();
    drop(first);
    let second = StateStore::open(&home).await.unwrap();
    drop(second);

    let database = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap();
    let (version, targets): (String, Vec<String>) = database
        .call(|connection| -> tokio_rusqlite::rusqlite::Result<_> {
            let version = connection.query_row(
                "SELECT value FROM metadata WHERE key = 'schema-version'",
                [],
                |row| row.get(0),
            )?;
            let targets = connection
                .prepare("SELECT target FROM target_route_state ORDER BY target")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok((version, targets))
        })
        .await
        .unwrap();
    assert_eq!(version, "4");
    assert_eq!(targets, ["claude", "codex"]);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn claude_target_view_projects_only_the_messages_preset() {
    let fixture = StoreFixture::new().await;
    let view = fixture.store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(view.target, Target::Claude);
    assert_eq!(view.route_health.state, "unobserved");
    assert_eq!(view.provider_presets.len(), 1);
    assert_eq!(view.provider_presets[0].key, "anthropic-api-messages");
    assert_eq!(
        view.provider_presets[0].protocol.to_string(),
        "anthropic-messages"
    );
    assert_eq!(
        view.provider_presets[0].authentication.to_string(),
        "anthropic-api-key"
    );
}

#[tokio::test]
async fn claude_provider_create_update_and_duplicate_preserve_its_declaration() {
    let fixture = StoreFixture::new().await;
    let created = fixture.store.apply_provider_action_for(Target::Claude, fixed_uuid(91), 0, serde_json::json!({
        "kind": "create-provider", "name": "Claude", "baseUrl": "https://api.anthropic.com/v1",
        "model": "claude-test", "credential": { "kind": "replace", "value": "secret" },
        "authentication": "anthropic-bearer", "presetKey": "anthropic-api-messages"
    })).await.unwrap();
    let provider = &created.view.providers[0];
    assert_eq!(provider.protocol.to_string(), "anthropic-messages");
    assert_eq!(provider.authentication.to_string(), "anthropic-bearer");
    let duplicate = fixture.store.apply_provider_action_for(Target::Claude, fixed_uuid(92), 1, serde_json::json!({
        "kind": "duplicate-provider", "sourceProviderId": provider.id, "sourceProviderRevision": 1,
        "name": "Claude copy", "baseUrl": "https://api.anthropic.com/v1", "model": "claude-test",
        "credential": { "kind": "reuse-source" }
    })).await.unwrap();
    assert_eq!(
        duplicate.view.providers[1].authentication.to_string(),
        "anthropic-bearer"
    );
    let updated = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            fixed_uuid(93),
            2,
            serde_json::json!({
                "kind": "update-provider", "providerId": provider.id, "providerRevision": 1,
                "name": "Claude", "baseUrl": "https://api.anthropic.com/v1", "model": "claude-test",
                "credential": { "kind": "keep" }, "authentication": "anthropic-api-key"
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        updated.view.providers[0].authentication.to_string(),
        "anthropic-api-key"
    );
    let api_key = fixture.store.apply_provider_action_for(Target::Claude, fixed_uuid(94), 3, serde_json::json!({
        "kind": "create-provider", "name": "Claude key", "baseUrl": "", "model": "", "credential": { "kind": "remove" },
        "authentication": "anthropic-api-key"
    })).await.unwrap();
    assert_eq!(
        api_key.view.providers[2].authentication.to_string(),
        "anthropic-api-key"
    );
    let rejected = fixture.store.apply_provider_action_for(Target::Claude, fixed_uuid(95), 4, serde_json::json!({
        "kind": "create-provider", "name": "Bad", "baseUrl": "", "model": "", "credential": { "kind": "remove" },
        "authentication": "openai-bearer"
    })).await.unwrap_err();
    assert_eq!(rejected.problem.code, "invalid-provider");
}

#[tokio::test]
async fn claude_preset_create_defaults_to_its_api_key_authentication() {
    let fixture = StoreFixture::new().await;
    let outcome = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            fixed_uuid(96),
            0,
            serde_json::json!({
                "kind": "create-provider", "name": "Claude preset",
                "baseUrl": "https://api.anthropic.com/v1", "model": "claude-test",
                "credential": { "kind": "remove" }, "presetKey": "anthropic-api-messages"
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.view.providers[0].authentication.to_string(),
        "anthropic-api-key"
    );
    let manual = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            fixed_uuid(98),
            1,
            serde_json::json!({
                "kind": "create-provider", "name": "Claude manual",
                "baseUrl": "https://api.example.test/v1", "model": "claude-test",
                "credential": { "kind": "remove" }
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(manual.problem.code, "invalid-provider");
}

#[tokio::test]
async fn malformed_claude_action_returns_claude_view_without_mutating_codex() {
    let fixture = StoreFixture::new().await;
    let codex_before = fixture.store.target_view().await.unwrap();
    let failure = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            fixed_uuid(97),
            0,
            serde_json::json!({ "kind": "bad" }),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.authoritative_view.target, Target::Claude);
    assert_eq!(
        fixture
            .store
            .target_view()
            .await
            .unwrap()
            .management_revision,
        codex_before.management_revision
    );
}

#[tokio::test]
async fn reopen_decodes_legacy_v4_claude_recovery_payload() {
    let root = std::env::temp_dir().join(format!("muxvia-legacy-v4-{}", Uuid::new_v4()));
    let home = MuxviaHome::from_user_home(&root);
    drop(StateStore::open(&home).await.unwrap());
    let id = Uuid::new_v4();
    let action_id = Uuid::new_v4();
    let database = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap();
    database.call(move |connection| {
        connection.execute(
            "INSERT INTO activation_recovery (id, target, action_id, config_path, file_identity_json, payload_json, state, created_revision) VALUES (?1, 'claude', ?2, '/tmp/claude', 'null', ?3, 'pending', 0)",
            tokio_rusqlite::rusqlite::params![id.to_string(), action_id.to_string(), r#"{"target":"claude","before":{"legacy":true},"desired":{"legacy":true}}"#],
        )?;
        Ok::<(), tokio_rusqlite::rusqlite::Error>(())
    }).await.unwrap();
    drop(database);
    let store = StateStore::open(&home).await.unwrap();
    assert_eq!(
        store
            .recovery_intent_for(Target::Claude, action_id)
            .await
            .unwrap()
            .unwrap()
            .target(),
        Target::Claude
    );
    drop(store);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn recovery_constructor_rejects_a_target_payload_mismatch() {
    let result = RecoveryIntent::pending_for_target(
        Target::Codex,
        Uuid::new_v4(),
        Uuid::new_v4(),
        PathBuf::from("/tmp/claude"),
        RecoveryPayload::ClaudeLegacy {
            payload: serde_json::json!({ "opaque": true }),
        },
        0,
    );
    assert!(result.is_err());
}

#[tokio::test]
async fn save_provider_persists_secret_separately_and_projects_no_secret() {
    let fixture = StoreFixture::new().await;
    let result = fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(10),
            0,
            raw_save_provider(
                "Local test",
                "http://127.0.0.1:4567/v1/",
                "gpt-test",
                "provider-secret-must-not-escape",
            ),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ActionStatus::Applied);
    assert_eq!(result.view.management_revision, 1);
    assert_eq!(
        result.view.providers[0].base_url,
        "http://127.0.0.1:4567/v1"
    );
    assert_eq!(
        result.view.providers[0].credential,
        CredentialPresence::Present
    );
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("provider-secret-must-not-escape")
    );

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let (credential, provider_secret_columns) = database
        .call(
            |connection| -> tokio_rusqlite::rusqlite::Result<(String, i64)> {
                let credential =
                    connection
                        .query_row("SELECT bearer_token FROM credentials", [], |row| row.get(0))?;
                let provider_secret_columns = connection.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('providers')
                 WHERE name IN ('bearer_token', 'credential')",
                    [],
                    |row| row.get(0),
                )?;
                Ok((credential, provider_secret_columns))
            },
        )
        .await
        .unwrap();
    assert_eq!(credential, "provider-secret-must-not-escape");
    assert_eq!(provider_secret_columns, 0);
}

#[tokio::test]
async fn receipt_lookup_replays_before_a_malformed_second_payload_is_examined() {
    let fixture = StoreFixture::new().await;
    let action_id = fixed_uuid(10);
    fixture
        .store
        .apply_save_provider_action(
            action_id,
            0,
            raw_save_provider(
                "Local test",
                "https://api.example.com/v1",
                "gpt-test",
                "first-secret",
            ),
        )
        .await
        .unwrap();
    let malformed_second_payload = serde_json::json!({
        "kind": "save-provider",
        "name": 42,
        "expectedRevision": 999
    });
    let replayed = fixture
        .store
        .apply_save_provider_action(action_id, 999, malformed_second_payload)
        .await
        .unwrap();

    assert_eq!(replayed.status, ActionStatus::Replayed);
    assert_eq!(replayed.view.management_revision, 1);
    assert_eq!(replayed.view.providers.len(), 1);
    assert_eq!(
        fixture.store.target_view().await.unwrap().providers.len(),
        1
    );
}

#[tokio::test]
async fn stale_revision_returns_authoritative_view_without_mutating_or_receipting() {
    let fixture = StoreFixture::new().await;
    fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(20),
            0,
            raw_save_provider(
                "First",
                "https://first.example/v1",
                "gpt-first",
                "first-secret",
            ),
        )
        .await
        .unwrap();
    let stale_action_id = fixed_uuid(21);

    let failure = fixture
        .store
        .apply_save_provider_action(
            stale_action_id,
            0,
            raw_save_provider(
                "Stale",
                "https://stale.example/v1",
                "gpt-stale",
                "stale-secret",
            ),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.problem.code, "stale-revision");
    assert_eq!(failure.authoritative_view.management_revision, 1);
    assert_eq!(failure.authoritative_view.providers.len(), 1);
    assert_eq!(
        fixture.store.target_view().await.unwrap().providers.len(),
        1
    );
    assert!(
        fixture
            .store
            .receipt(stale_action_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn new_malformed_raw_action_rejects_without_receipt_or_secret_echo() {
    let fixture = StoreFixture::new().await;
    let action_id = fixed_uuid(22);
    let secret = "malformed-secret-must-not-escape";
    let failure = fixture
        .store
        .apply_save_provider_action(
            action_id,
            0,
            serde_json::json!({
                "kind": "save-provider",
                "name": 42,
                "credential": secret
            }),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.problem.code, "invalid-provider");
    assert_eq!(failure.authoritative_view.management_revision, 0);
    assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    assert!(!format!("{failure:?}\n{failure}").contains(secret));
}

#[tokio::test]
async fn non_save_raw_actions_reject_without_consuming_action_ids() {
    let fixture = StoreFixture::new().await;
    let activate_id = fixed_uuid(23);
    let activate = fixture
        .store
        .apply_save_provider_action(
            activate_id,
            0,
            serde_json::json!({
                "kind": "activate-provider",
                "providerId": "00000000-0000-4000-8000-000000000001",
                "mode": "takeover"
            }),
        )
        .await
        .unwrap_err();
    let unknown_id = fixed_uuid(24);
    let unknown = fixture
        .store
        .apply_save_provider_action(
            unknown_id,
            0,
            serde_json::json!({ "kind": "delete-provider" }),
        )
        .await
        .unwrap_err();

    assert_eq!(activate.problem.code, "unsupported-operation");
    assert_eq!(unknown.problem.code, "invalid-provider");
    assert!(fixture.store.receipt(activate_id).await.unwrap().is_none());
    assert!(fixture.store.receipt(unknown_id).await.unwrap().is_none());
}

#[test]
fn provider_urls_normalize_https_and_all_loopback_http_hosts() {
    let cases = [
        (
            "https://api.example.com/v1///",
            "https://api.example.com/v1",
        ),
        ("http://127.42.5.9:4567/v1/", "http://127.42.5.9:4567/v1"),
        ("http://localhost:4567/v1/", "http://localhost:4567/v1"),
        ("http://[::1]:4567/v1/", "http://[::1]:4567/v1"),
    ];

    for (input, expected) in cases {
        assert_eq!(normalize_provider_base_url(input).unwrap(), expected);
    }
}

#[tokio::test]
async fn unsafe_provider_urls_reject_without_consuming_action_ids() {
    let fixture = StoreFixture::new().await;
    let invalid_urls = [
        "http://api.example.com/v1",
        "https://operator@api.example.com/v1",
        "https://api.example.com/v1?model=gpt-test",
        "https://api.example.com/v1#responses",
    ];

    for (index, base_url) in invalid_urls.into_iter().enumerate() {
        let action_id = fixed_uuid(30 + index as u8);
        let failure = fixture
            .store
            .apply_save_provider_action(
                action_id,
                0,
                raw_save_provider("Unsafe", base_url, "gpt-test", "unsafe-secret"),
            )
            .await
            .unwrap_err();

        assert_eq!(failure.problem.code, "invalid-provider", "{base_url}");
        assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
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
async fn only_invalid_provider_fields_reject_while_missing_model_persists() {
    let fixture = StoreFixture::new().await;
    let cases = [
        ("", "gpt-test", "provider-secret"),
        ("Local", "gpt-test", ""),
    ];

    for (index, (name, model, credential)) in cases.into_iter().enumerate() {
        let action_id = fixed_uuid(40 + index as u8);
        let failure = fixture
            .store
            .apply_save_provider_action(
                action_id,
                0,
                raw_save_provider(name, "https://api.example.com/v1", model, credential),
            )
            .await
            .unwrap_err();

        assert_eq!(failure.problem.code, "invalid-provider");
        assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    }
    let incomplete = fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(42),
            0,
            raw_save_provider("Local", "https://api.example.com/v1", "", "provider-secret"),
        )
        .await
        .unwrap();
    assert_eq!(
        incomplete.view.providers[0].credential,
        CredentialPresence::Present
    );
    assert_eq!(incomplete.view.providers[0].model, "");
}

#[tokio::test]
async fn opening_is_read_only_and_saving_increments_revision_and_sequence_together() {
    let fixture = StoreFixture::new().await;
    let opened = fixture.store.target_view().await.unwrap();
    assert_eq!(opened.management_revision, 0);
    assert_eq!(opened.view_sequence, 0);
    assert!(opened.providers.is_empty());

    let saved = fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(50),
            0,
            raw_save_provider(
                "Local",
                "https://api.example.com/v1",
                "gpt-test",
                "counter-secret",
            ),
        )
        .await
        .unwrap();

    assert_eq!(saved.view.management_revision, 1);
    assert_eq!(saved.view.view_sequence, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn muxvia_home_directories_and_credential_database_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = StoreFixture::new().await;
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

    assert_eq!(mode(fixture.home.root()), 0o700);
    assert_eq!(mode(fixture.home.state_dir()), 0o700);
    assert_eq!(mode(fixture.home.database_path()), 0o600);
}

#[tokio::test]
async fn target_views_receipts_and_failures_never_render_persisted_secrets() {
    let fixture = StoreFixture::new().await;
    for (index, secret) in ["persisted-secret-one", "persisted-secret-two"]
        .into_iter()
        .enumerate()
    {
        fixture
            .store
            .apply_save_provider_action(
                fixed_uuid(60 + index as u8),
                index as u64,
                raw_save_provider(
                    &format!("Provider {index}"),
                    &format!("https://api{index}.example.com/v1"),
                    &format!("gpt-{index}"),
                    secret,
                ),
            )
            .await
            .unwrap();
    }
    let failure = fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(62),
            0,
            raw_save_provider(
                "Stale",
                "https://stale.example.com/v1",
                "gpt-stale",
                "not-persisted-secret",
            ),
        )
        .await
        .unwrap_err();
    let view = fixture.store.target_view().await.unwrap();
    let receipt = fixture
        .store
        .receipt(fixed_uuid(60))
        .await
        .unwrap()
        .unwrap();
    let rendered = format!(
        "{}\n{:?}\n{}\n{:?}\n{:?}",
        serde_json::to_string(&view).unwrap(),
        view,
        failure,
        failure,
        receipt
    );

    assert!(!rendered.contains("persisted-secret-one"));
    assert!(!rendered.contains("persisted-secret-two"));
    assert!(!rendered.contains("not-persisted-secret"));
}

#[tokio::test]
async fn concurrent_duplicate_action_ids_apply_once_and_replay_once() {
    let fixture = StoreFixture::new().await;
    let store = fixture.store.clone();
    let action_id = fixed_uuid(70);
    let first = store.apply_save_provider_action(
        action_id,
        0,
        raw_save_provider(
            "First arrival",
            "https://first.example.com/v1",
            "gpt-first",
            "concurrent-secret-one",
        ),
    );
    let second = store.apply_save_provider_action(
        action_id,
        0,
        raw_save_provider(
            "Second arrival",
            "https://second.example.com/v1",
            "gpt-second",
            "concurrent-secret-two",
        ),
    );

    let (first, second) = tokio::join!(first, second);
    let statuses = [first.unwrap().status, second.unwrap().status];

    assert!(statuses.contains(&ActionStatus::Applied));
    assert!(statuses.contains(&ActionStatus::Replayed));
    assert_eq!(store.target_view().await.unwrap().providers.len(), 1);
}

#[tokio::test]
async fn target_scoped_receipts_and_recovery_action_ids_do_not_cross_targets() {
    let fixture = StoreFixture::new().await;
    let action_id = fixed_uuid(71);
    let codex = fixture.store.target_view().await.unwrap();
    let mut claude = codex.clone();
    claude.target = Target::Claude;
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
            for (target, outcome) in [("codex", codex), ("claude", claude)] {
                connection.execute(
                    "INSERT INTO action_receipts
                     (target, action_id, action_kind, committed_revision, outcome_json)
                     VALUES (?1, ?2, 'create-provider', 0, ?3)",
                    tokio_rusqlite::rusqlite::params![
                        target,
                        action_id.to_string(),
                        serde_json::to_string(&muxvia_routing::control::protocol::ActionOutcome {
                            status: ActionStatus::Applied,
                            view: outcome,
                        })
                        .unwrap(),
                    ],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

    let codec = CodexConfigCodec::for_user_home(fixture.home.user_home()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    fs::write(codec.config_path(), "approval_policy = \"never\"\n").unwrap();
    let codex_intent = RecoveryIntent::pending(
        Uuid::new_v4(),
        action_id,
        codec.config_path().to_owned(),
        codec.inspect().unwrap(),
        codec.desired_direct("gpt-test", "https://api.openai.com/v1", "openai"),
        0,
    );
    let claude_intent = RecoveryIntent::pending_for_target(
        Target::Claude,
        Uuid::new_v4(),
        action_id,
        PathBuf::from("/tmp/claude-settings"),
        RecoveryPayload::ClaudeLegacy {
            payload: serde_json::json!({ "owned": "claude" }),
        },
        0,
    )
    .unwrap();
    fixture
        .store
        .insert_recovery_intent(&codex_intent)
        .await
        .unwrap();
    fixture
        .store
        .insert_recovery_intent(&claude_intent)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .store
            .receipt_for(Target::Codex, action_id)
            .await
            .unwrap()
            .unwrap()
            .view
            .target,
        Target::Codex
    );
    assert_eq!(
        fixture
            .store
            .receipt_for(Target::Claude, action_id)
            .await
            .unwrap()
            .unwrap()
            .view
            .target,
        Target::Claude
    );
    assert_eq!(
        fixture
            .store
            .recovery_intent_for(Target::Codex, action_id)
            .await
            .unwrap()
            .unwrap()
            .id(),
        codex_intent.id()
    );
    assert_eq!(
        fixture
            .store
            .recovery_intent_for(Target::Claude, action_id)
            .await
            .unwrap()
            .unwrap()
            .id(),
        claude_intent.id()
    );
    let targets = fixture
        .store
        .pending_recovery_intents()
        .await
        .unwrap()
        .into_iter()
        .map(|intent| intent.target())
        .collect::<Vec<_>>();
    assert_eq!(targets, vec![Target::Codex, Target::Claude]);
}

#[test]
fn typed_and_raw_action_debug_output_redacts_credentials() {
    let secret = "debug-secret-must-not-escape";
    let action = TargetAction::CreateProvider {
        name: "Local".into(),
        base_url: "https://api.example.com/v1".into(),
        model: "gpt-test".into(),
        credential: CredentialEdit::Replace {
            value: secret.into(),
        },
        authentication: None,
        preset_key: None,
    };
    let operation = ControlOperation::Act {
        target: Target::Codex,
        action_id: fixed_uuid(80),
        expected_revision: 0,
        action: serde_json::to_value(&action).unwrap(),
    };
    let rendered_action = format!("{action:?}");
    let rendered_operation = format!("{operation:?}");
    let rendered_frame = format!(
        "{:?}",
        ClientFrame::Request {
            request_id: "request-80".into(),
            operation,
        }
    );

    for rendered in [rendered_action, rendered_operation, rendered_frame] {
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("<redacted>"));
    }
}
