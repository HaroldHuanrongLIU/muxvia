use std::{fs, path::PathBuf};

use muxvia_routing::{
    control::protocol::{
        ActionStatus, ClientFrame, ControlOperation, CredentialPresence, Target, TargetAction,
    },
    domain::provider::normalize_provider_base_url,
    home::MuxviaHome,
    state::{SaveProviderCommand, StateStore},
};
use secrecy::SecretString;
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

#[tokio::test]
async fn save_provider_persists_secret_separately_and_projects_no_secret() {
    let fixture = StoreFixture::new().await;
    let result = fixture
        .store
        .save_provider(SaveProviderCommand {
            action_id: fixed_uuid(10),
            expected_revision: 0,
            name: "Local test".into(),
            base_url: "http://127.0.0.1:4567/v1/".into(),
            model: "gpt-test".into(),
            credential: SecretString::from("provider-secret-must-not-escape"),
        })
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
                let credential = connection.query_row(
                    "SELECT bearer_token FROM provider_credentials",
                    [],
                    |row| row.get(0),
                )?;
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
        .save_provider(SaveProviderCommand {
            action_id,
            expected_revision: 0,
            name: "Local test".into(),
            base_url: "https://api.example.com/v1".into(),
            model: "gpt-test".into(),
            credential: SecretString::from("first-secret"),
        })
        .await
        .unwrap();
    let malformed_second_payload = serde_json::json!({
        "kind": "save-provider",
        "name": 42,
        "expectedRevision": 999
    });
    assert!(
        serde_json::from_value::<muxvia_routing::control::protocol::TargetAction>(
            malformed_second_payload
        )
        .is_err()
    );

    let replayed = fixture.store.receipt(action_id).await.unwrap().unwrap();

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
        .save_provider(SaveProviderCommand {
            action_id: fixed_uuid(20),
            expected_revision: 0,
            name: "First".into(),
            base_url: "https://first.example/v1".into(),
            model: "gpt-first".into(),
            credential: SecretString::from("first-secret"),
        })
        .await
        .unwrap();
    let stale_action_id = fixed_uuid(21);

    let failure = fixture
        .store
        .save_provider(SaveProviderCommand {
            action_id: stale_action_id,
            expected_revision: 0,
            name: "Stale".into(),
            base_url: "https://stale.example/v1".into(),
            model: "gpt-stale".into(),
            credential: SecretString::from("stale-secret"),
        })
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
            .save_provider(SaveProviderCommand {
                action_id,
                expected_revision: 0,
                name: "Unsafe".into(),
                base_url: base_url.into(),
                model: "gpt-test".into(),
                credential: SecretString::from("unsafe-secret"),
            })
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
async fn incomplete_provider_fields_reject_without_consuming_action_ids() {
    let fixture = StoreFixture::new().await;
    let cases = [
        ("", "gpt-test", "provider-secret"),
        ("Local", "", "provider-secret"),
        ("Local", "gpt-test", ""),
    ];

    for (index, (name, model, credential)) in cases.into_iter().enumerate() {
        let action_id = fixed_uuid(40 + index as u8);
        let failure = fixture
            .store
            .save_provider(SaveProviderCommand {
                action_id,
                expected_revision: 0,
                name: name.into(),
                base_url: "https://api.example.com/v1".into(),
                model: model.into(),
                credential: SecretString::from(credential),
            })
            .await
            .unwrap_err();

        assert_eq!(failure.problem.code, "incomplete-provider");
        assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    }
    assert_eq!(
        fixture.store.target_view().await.unwrap().providers.len(),
        0
    );
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
        .save_provider(SaveProviderCommand {
            action_id: fixed_uuid(50),
            expected_revision: 0,
            name: "Local".into(),
            base_url: "https://api.example.com/v1".into(),
            model: "gpt-test".into(),
            credential: SecretString::from("counter-secret"),
        })
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
            .save_provider(SaveProviderCommand {
                action_id: fixed_uuid(60 + index as u8),
                expected_revision: index as u64,
                name: format!("Provider {index}"),
                base_url: format!("https://api{index}.example.com/v1"),
                model: format!("gpt-{index}"),
                credential: SecretString::from(secret),
            })
            .await
            .unwrap();
    }
    let failure = fixture
        .store
        .save_provider(SaveProviderCommand {
            action_id: fixed_uuid(62),
            expected_revision: 0,
            name: "Stale".into(),
            base_url: "https://stale.example.com/v1".into(),
            model: "gpt-stale".into(),
            credential: SecretString::from("not-persisted-secret"),
        })
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
    let first = store.save_provider(SaveProviderCommand {
        action_id,
        expected_revision: 0,
        name: "First arrival".into(),
        base_url: "https://first.example.com/v1".into(),
        model: "gpt-first".into(),
        credential: SecretString::from("concurrent-secret-one"),
    });
    let second = store.save_provider(SaveProviderCommand {
        action_id,
        expected_revision: 0,
        name: "Second arrival".into(),
        base_url: "https://second.example.com/v1".into(),
        model: "gpt-second".into(),
        credential: SecretString::from("concurrent-secret-two"),
    });

    let (first, second) = tokio::join!(first, second);
    let statuses = [first.unwrap().status, second.unwrap().status];

    assert!(statuses.contains(&ActionStatus::Applied));
    assert!(statuses.contains(&ActionStatus::Replayed));
    assert_eq!(store.target_view().await.unwrap().providers.len(), 1);
}

#[test]
fn typed_and_raw_action_debug_output_redacts_credentials() {
    let secret = "debug-secret-must-not-escape";
    let action = TargetAction::SaveProvider {
        name: "Local".into(),
        base_url: "https://api.example.com/v1".into(),
        model: "gpt-test".into(),
        credential: secret.into(),
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
