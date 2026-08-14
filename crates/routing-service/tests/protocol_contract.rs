use std::path::PathBuf;

use muxvia_routing::control::{
    framing::{FrameError, read_frame, write_frame},
    protocol::{
        ClientFrame, CredentialEdit, ProviderCompleteness, ProviderProtocol, ProviderRequirement,
        ServerFrame, TargetAction, TargetView,
    },
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/fixtures")
        .join(name);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn fixtures_round_trip_as_their_protocol_types() {
    let hello = fixture("hello.json");
    let client: ClientFrame = serde_json::from_value(hello.clone()).unwrap();
    assert_eq!(serde_json::to_value(client).unwrap(), hello);

    let target_view = fixture("initial-target-view.json");
    let view: TargetView = serde_json::from_value(target_view.clone()).unwrap();
    assert_eq!(serde_json::to_value(view).unwrap(), target_view);

    let save_provider = fixture("save-provider.json");
    let action: TargetAction = serde_json::from_value(save_provider.clone()).unwrap();
    assert_eq!(serde_json::to_value(action).unwrap(), save_provider);

    for name in ["reorder-providers.json", "delete-provider.json"] {
        let action = fixture(name);
        let parsed: TargetAction = serde_json::from_value(action.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), action);
    }
}

#[test]
fn provider_lifecycle_actions_use_revision_guarded_secret_free_wire_shapes() {
    let reorder = TargetAction::ReorderProviders {
        provider_ids: vec![
            "00000000-0000-4000-8000-000000000103".parse().unwrap(),
            "00000000-0000-4000-8000-000000000101".parse().unwrap(),
            "00000000-0000-4000-8000-000000000102".parse().unwrap(),
        ],
    };
    assert_eq!(
        serde_json::to_value(reorder).unwrap(),
        fixture("reorder-providers.json")
    );

    let delete = TargetAction::DeleteProvider {
        provider_id: "00000000-0000-4000-8000-000000000101".parse().unwrap(),
        provider_revision: 7,
    };
    assert_eq!(
        serde_json::to_value(delete).unwrap(),
        fixture("delete-provider.json")
    );
}

#[test]
fn provider_action_revisions_must_be_positive_on_the_wire() {
    for action in [
        serde_json::json!({
            "kind": "update-provider",
            "providerId": "00000000-0000-4000-8000-000000000101",
            "providerRevision": 0,
            "name": "Provider",
            "baseUrl": "https://provider.example/v1",
            "model": "model-test",
            "credential": { "kind": "keep" },
        }),
        serde_json::json!({
            "kind": "delete-provider",
            "providerId": "00000000-0000-4000-8000-000000000101",
            "providerRevision": 0,
        }),
    ] {
        assert!(serde_json::from_value::<TargetAction>(action).is_err());
    }
}

#[test]
fn provider_declaration_contract_round_trips_the_secret_free_projection_and_actions() {
    let provider = serde_json::json!({
        "id": "00000000-0000-4000-8000-000000000101",
        "position": 0,
        "providerRevision": 1,
        "name": "Incomplete",
        "baseUrl": "",
        "model": "",
        "protocol": "openai-responses",
        "credential": "missing",
        "completeness": "incomplete",
        "missingFields": ["base-url", "model", "credential"],
        "provenance": null,
        "generated": false,
        "activeReferences": []
    });
    let view = serde_json::json!({
        "target": "codex",
        "managementRevision": 0,
        "viewSequence": 0,
        "service": { "epoch": "00000000-0000-4000-8000-000000000001", "state": "running" },
        "mode": "unmanaged",
        "takeover": { "state": "inactive", "endpoint": null },
        "providers": [provider.clone()],
        "providerPresets": [{
            "key": "openai-api-responses",
            "baseUrl": "https://api.openai.com/v1",
            "model": "",
            "protocol": "openai-responses"
        }],
        "currentProviderId": null,
        "servingProviderId": null,
        "managedConfiguration": { "state": "unmanaged", "path": null, "restartRequired": false },
        "recovery": { "intentId": null, "state": "clean" },
        "activatedSnapshot": null,
        "problems": []
    });
    let parsed: TargetView = serde_json::from_value(view.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), view);

    assert_eq!(
        ProviderProtocol::OpenaiResponses.to_string(),
        "openai-responses"
    );
    assert_eq!(ProviderCompleteness::Incomplete.to_string(), "incomplete");
    assert_eq!(ProviderRequirement::BaseUrl.to_string(), "base-url");

    let action = TargetAction::CreateProvider {
        name: "Incomplete".into(),
        base_url: String::new(),
        model: String::new(),
        credential: CredentialEdit::Remove,
        preset_key: Some("openai-api-responses".into()),
    };
    assert_eq!(
        serde_json::to_value(action).unwrap(),
        serde_json::json!({
            "kind": "create-provider",
            "name": "Incomplete",
            "baseUrl": "",
            "model": "",
            "credential": { "kind": "remove" },
            "presetKey": "openai-api-responses"
        })
    );
}

#[test]
fn credential_edit_debug_redacts_replacement_values_and_wire_types_accept_unknown_fields() {
    let sentinel = "credential-sentinel-must-not-escape";
    assert!(
        !format!(
            "{:?}",
            CredentialEdit::Replace {
                value: sentinel.into()
            }
        )
        .contains(sentinel)
    );

    let action: TargetAction = serde_json::from_value(serde_json::json!({
        "kind": "create-provider",
        "name": "Incomplete",
        "baseUrl": "",
        "model": "",
        "credential": { "kind": "remove" },
        "futureField": "ignored"
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(action).unwrap(),
        serde_json::json!({
            "kind": "create-provider",
            "name": "Incomplete",
            "baseUrl": "",
            "model": "",
            "credential": { "kind": "remove" },
            "presetKey": null
        })
    );
}

#[tokio::test]
async fn framing_rejects_invalid_utf8() {
    let (mut writer, mut reader) = tokio::io::duplex(32);
    writer.write_all(&[0, 0, 0, 1, 0xff]).await.unwrap();
    drop(writer);

    assert_eq!(
        read_frame(&mut reader).await.unwrap_err(),
        FrameError::InvalidUtf8
    );
}

#[tokio::test]
async fn framing_rejects_invalid_json() {
    let (mut writer, mut reader) = tokio::io::duplex(32);
    writer.write_all(&[0, 0, 0, 1, b'{']).await.unwrap();
    drop(writer);

    assert_eq!(
        read_frame(&mut reader).await.unwrap_err(),
        FrameError::InvalidJson
    );
}

#[tokio::test]
async fn framing_rejects_partial_eof() {
    let (mut writer, mut reader) = tokio::io::duplex(32);
    writer.write_all(&[0, 0, 0, 4, b'{', b'}']).await.unwrap();
    drop(writer);

    assert_eq!(
        read_frame(&mut reader).await.unwrap_err(),
        FrameError::UnexpectedEof
    );
}

#[tokio::test]
async fn framing_writes_a_big_endian_length_prefix() {
    let (mut writer, mut reader) = tokio::io::duplex(64);
    let value = serde_json::json!({ "type": "hello" });
    write_frame(&mut writer, &value).await.unwrap();
    drop(writer);

    let mut encoded = Vec::new();
    reader.read_to_end(&mut encoded).await.unwrap();
    assert_eq!(&encoded[..4], &(encoded.len() as u32 - 4).to_be_bytes());
}

#[test]
fn target_projections_do_not_serialize_secrets() {
    let mut value = fixture("initial-target-view.json");
    value["activatedSnapshot"] = serde_json::json!({
        "id": "00000000-0000-4000-8000-000000000002",
        "providerId": "00000000-0000-4000-8000-000000000003",
        "model": "gpt-test",
        "epoch": "00000000-0000-4000-8000-000000000004",
        "providerCredential": "provider-secret-must-not-escape",
        "routingCredential": "routing-secret-must-not-escape",
        "authorization": "Bearer provider-secret-must-not-escape",
        "recovery": { "raw": "routing-secret-must-not-escape" }
    });
    value["problems"] = serde_json::json!([{
        "code": "invalid-action",
        "message": "The action cannot be completed.",
        "providerCredential": "provider-secret-must-not-escape",
        "routingCredential": "routing-secret-must-not-escape"
    }]);
    value["providerCredential"] = serde_json::json!("provider-secret-must-not-escape");

    let view: TargetView = serde_json::from_value(value).unwrap();
    let serialized = serde_json::to_string(&view).unwrap();

    assert!(!serialized.contains("provider-secret-must-not-escape"));
    assert!(!serialized.contains("routing-secret-must-not-escape"));
    assert_eq!(
        serde_json::from_str::<Value>(&serialized).unwrap()["activatedSnapshot"],
        serde_json::json!({
            "id": "00000000-0000-4000-8000-000000000002",
            "providerId": "00000000-0000-4000-8000-000000000003",
            "model": "gpt-test",
            "epoch": "00000000-0000-4000-8000-000000000004"
        })
    );
}

#[test]
fn protocol_literals_and_identifiers_are_validated() {
    let invalid_action_id = serde_json::json!({
        "type": "request",
        "requestId": "request-1",
        "operation": {
            "kind": "act",
            "target": "codex",
            "actionId": "not-a-uuid",
            "expectedRevision": 0,
            "action": { "kind": "create-provider" }
        }
    });
    assert!(serde_json::from_value::<ClientFrame>(invalid_action_id).is_err());

    let hello_ack = serde_json::json!({
        "type": "hello-ack",
        "rpc": { "major": 1, "minor": 0 },
        "release": "test",
        "serviceEpoch": "00000000-0000-4000-8000-000000000001",
        "frameLimit": 1048576
    });
    assert!(serde_json::from_value::<ServerFrame>(hello_ack.clone()).is_ok());
    let mut invalid_rpc = hello_ack.clone();
    invalid_rpc["rpc"] = serde_json::json!({ "major": 1, "minor": 1 });
    assert!(serde_json::from_value::<ServerFrame>(invalid_rpc).is_err());

    let mut invalid_frame_limit = hello_ack;
    invalid_frame_limit["frameLimit"] = serde_json::json!(1048575);
    assert!(serde_json::from_value::<ServerFrame>(invalid_frame_limit).is_err());
}
