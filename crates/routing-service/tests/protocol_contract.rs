use std::path::PathBuf;

use muxvia_routing::{
    control::{
        framing::{FrameError, read_frame, write_frame},
        protocol::{
            ActivationMode, ClientFrame, ControlResult, CredentialEdit, DiscoverySource,
            DraftCredentialSource, DuplicateCredential, ProviderAuthentication,
            ProviderCompleteness, ProviderProtocol, ProviderRequirement,
            ProviderRoutingRequirement, ServerFrame, Target, TargetAction, TargetView,
            UniversalProviderAction,
        },
    },
    domain::provider::has_valid_provider_declaration,
    service::provider_inspector::{DiscoveredModel, ModelDiscoveryResult, ReachabilityResult},
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

    for name in [
        "reorder-providers.json",
        "delete-provider.json",
        "duplicate-provider.json",
    ] {
        let action = fixture(name);
        let parsed: TargetAction = serde_json::from_value(action.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), action);
    }

    for name in [
        "discover-models.json",
        "check-reachability.json",
        "cancel-inspection.json",
        "probe-compatibility.json",
        "open-universal-providers.json",
    ] {
        let frame = fixture(name);
        let parsed: ClientFrame = serde_json::from_value(frame.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), frame);
    }

    let compatibility_probe = fixture("compatibility-probe.json");
    let parsed: ServerFrame = serde_json::from_value(compatibility_probe.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), compatibility_probe);

    let universal_catalog = fixture("universal-provider-catalog.json");
    let parsed: ServerFrame = serde_json::from_value(universal_catalog.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), universal_catalog);

    for name in [
        "universal-provider-act.json",
        "universal-provider-outcome.json",
        "universal-provider-view.json",
    ] {
        let frame = fixture(name);
        if name == "universal-provider-act.json" {
            let parsed: ClientFrame = serde_json::from_value(frame.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), frame);
        } else {
            let parsed: ServerFrame = serde_json::from_value(frame.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), frame);
        }
    }

    let universal_create = fixture("create-universal-provider.json");
    let parsed: UniversalProviderAction = serde_json::from_value(universal_create.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), universal_create);
    for name in [
        "update-universal-provider.json",
        "duplicate-universal-provider.json",
        "delete-universal-provider.json",
        "synchronize-universal-provider.json",
    ] {
        let action = fixture(name);
        let parsed: UniversalProviderAction = serde_json::from_value(action.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), action);
    }

    let resolve_compatibility = fixture("resolve-compatibility.json");
    let parsed: TargetAction = serde_json::from_value(resolve_compatibility.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), resolve_compatibility);
}

#[test]
fn universal_provider_contracts_are_closed_schema_complete_and_secret_free() {
    let schema = fixture("../control-v1.schema.json");
    let has_branch = |definition: &str, discriminator: &str| {
        schema["$defs"][definition]["oneOf"]
            .as_array()
            .is_some_and(|branches| {
                branches.iter().any(|branch| {
                    branch["properties"]["kind"]["const"] == discriminator
                        || branch["properties"]["type"]["const"] == discriminator
                })
            })
    };
    assert!(has_branch("controlOperation", "open-universal-providers"));
    assert!(has_branch("controlOperation", "universal-provider-act"));
    assert!(has_branch("controlResult", "universal-provider-catalog"));
    assert!(has_branch("controlResult", "universal-provider-outcome"));
    assert!(has_branch("serverFrame", "universal-provider-view"));
    assert!(schema["$defs"].get("universalProviderAction").is_some());
    assert!(schema["$defs"].get("universalProviderCatalog").is_some());
    assert!(
        schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .any(|branch| { branch["$ref"] == "#/$defs/universalProviderAction" })
    );

    let mut action = fixture("create-universal-provider.json");
    action["additiveSecret"] = serde_json::json!("UNIVERSAL_ADDITIVE_SECRET_99310");
    assert!(
        serde_json::from_value::<UniversalProviderAction>(action).is_err(),
        "accepted additive Universal Provider action field"
    );
    let parsed: UniversalProviderAction =
        serde_json::from_value(fixture("create-universal-provider.json")).unwrap();
    let diagnostic = format!("{parsed:?}");
    assert!(
        !diagnostic.contains("universal-provider-secret-must-not-escape"),
        "debugged Universal Provider secret"
    );
}

// Catches a protocol mutation that accepts arbitrary reconciliation discriminators,
// loses the opaque observation token, or retains additive secret-bearing fields.
#[test]
fn reconciliation_preview_and_apply_contracts_are_closed_and_secret_free() {
    let mut preview = fixture("preview-reconciliation.json");
    preview["result"]["preview"]["providerCredential"] =
        serde_json::json!("preview-secret-must-not-escape");
    preview["result"]["preview"]["changes"][0]["rawConfiguration"] =
        serde_json::json!("preview-secret-must-not-escape");

    let parsed: ServerFrame = serde_json::from_value(preview).unwrap();
    let serialized = serde_json::to_string(&parsed).unwrap();
    assert!(!serialized.contains("preview-secret-must-not-escape"));
    assert!(!format!("{parsed:?}").contains("preview-secret-must-not-escape"));

    let expected = fixture("preview-reconciliation.json");
    assert_eq!(serde_json::to_value(parsed).unwrap(), expected);

    let apply = fixture("apply-reconciliation.json");
    let action: TargetAction = serde_json::from_value(apply.clone()).unwrap();
    assert_eq!(serde_json::to_value(action).unwrap(), apply);

    let valid_preview = fixture("preview-reconciliation.json");
    for (path, invalid_value) in [
        (
            "/result/preview/observationToken",
            serde_json::json!("not-a-uuid"),
        ),
        ("/result/preview/managementRevision", serde_json::json!(0)),
        (
            "/result/preview/compatibility/classification",
            serde_json::json!("arbitrary"),
        ),
        (
            "/result/preview/shadowSources/0",
            serde_json::json!("arbitrary-source"),
        ),
        (
            "/result/preview/changes/0/field",
            serde_json::json!("arbitrary-field"),
        ),
        (
            "/result/preview/changes/0/state",
            serde_json::json!("arbitrary-state"),
        ),
    ] {
        let mut invalid = valid_preview.clone();
        *invalid.pointer_mut(path).unwrap() = invalid_value;
        assert!(
            serde_json::from_value::<ServerFrame>(invalid).is_err(),
            "accepted invalid reconciliation preview {path}"
        );
    }
    let invalid_operation = serde_json::json!({
        "type": "request", "requestId": "preview", "operation": {
            "kind": "preview-reconciliation", "target": "codex", "strategy": "automatic"
        }
    });
    assert!(serde_json::from_value::<ClientFrame>(invalid_operation).is_err());

    for (field, invalid_value) in [
        ("strategy", serde_json::json!("automatic")),
        ("observationToken", serde_json::json!("not-a-uuid")),
    ] {
        let mut invalid = fixture("apply-reconciliation.json");
        invalid[field] = invalid_value;
        assert!(
            serde_json::from_value::<TargetAction>(invalid).is_err(),
            "accepted invalid reconciliation action {field}"
        );
    }
}

#[test]
fn compatibility_probe_and_resolution_contracts_are_exact_closed_and_secret_free() {
    let probe_request = fixture("probe-compatibility.json");
    let probe_response = fixture("compatibility-probe.json");
    let resolution = fixture("resolve-compatibility.json");

    for (mut invalid, pointer) in [
        (probe_request.clone(), "/operation"),
        (probe_response.clone(), "/result"),
        (probe_response.clone(), "/result/probe"),
        (resolution.clone(), ""),
    ] {
        let object = if pointer.is_empty() {
            invalid.as_object_mut().unwrap()
        } else {
            invalid
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
        };
        object.insert(
            "additiveSecret".into(),
            serde_json::json!("COMPATIBILITY_PROTOCOL_SECRET_98711"),
        );
        let rejected = if pointer == "/operation" {
            serde_json::from_value::<ClientFrame>(invalid).is_err()
        } else if pointer.starts_with("/result") {
            serde_json::from_value::<ServerFrame>(invalid).is_err()
        } else {
            serde_json::from_value::<TargetAction>(invalid).is_err()
        };
        assert!(rejected, "accepted additive compatibility protocol field");
    }

    for legacy in [
        serde_json::json!({"kind": "preview-compatibility", "target": "codex"}),
        serde_json::json!({"kind": "acknowledge-compatibility", "version": "0.42.0"}),
    ] {
        assert!(
            serde_json::from_value::<muxvia_routing::control::protocol::ControlOperation>(
                legacy.clone()
            )
            .is_err()
                && serde_json::from_value::<TargetAction>(legacy).is_err(),
            "accepted removed compatibility discriminator"
        );
    }

    let schema = fixture("../control-v1.schema.json");
    let operation_branches = schema["$defs"]["controlOperation"]["oneOf"]
        .as_array()
        .unwrap();
    let result_branches = schema["$defs"]["controlResult"]["oneOf"]
        .as_array()
        .unwrap();
    let action_branches = schema["$defs"]["targetAction"]["oneOf"].as_array().unwrap();
    assert!(operation_branches.iter().any(|branch| {
        branch["properties"]["kind"]["const"] == "probe-compatibility"
            && branch["additionalProperties"] == false
    }));
    assert!(result_branches.iter().any(|branch| {
        branch["properties"]["kind"]["const"] == "compatibility-probe"
            && branch["additionalProperties"] == false
    }));
    assert!(action_branches.iter().any(|branch| {
        branch["properties"]["kind"]["const"] == "resolve-compatibility"
            && branch["additionalProperties"] == false
    }));
    assert_eq!(
        schema["$defs"]["controlProblem"]["properties"]["selector"]["$ref"],
        "#/$defs/claudeBlockingSelector"
    );
    assert_eq!(
        schema["$defs"]["controlProblem"]["properties"]["source"]["enum"],
        serde_json::json!([
            "control-plane-context",
            "user-settings",
            "managed-settings",
            "shared-project-settings",
            "local-project-settings",
            "codex-profile",
            "claude-managed",
            "claude-shared",
            "claude-project",
            "claude-local",
            "claude-selector",
            "claude-host-managed"
        ])
    );
}

#[test]
fn inspection_protocol_round_trips_all_sources_cancellation_and_view_free_results() {
    let saved = DiscoverySource::Saved {
        provider_id: "00000000-0000-4000-8000-000000000101".parse().unwrap(),
        provider_revision: 7,
    };
    let sources = [
        saved,
        DiscoverySource::Draft {
            base_url: "https://draft.example/v1".into(),
            authentication: ProviderAuthentication::OpenaiBearer,
            credential_source: DraftCredentialSource::Missing,
        },
        DiscoverySource::Draft {
            base_url: "https://draft.example/v1?token=endpoint-query-must-not-escape".into(),
            authentication: ProviderAuthentication::AnthropicBearer,
            credential_source: DraftCredentialSource::Ephemeral {
                value: "ephemeral-secret-must-not-escape".into(),
            },
        },
        DiscoverySource::Draft {
            base_url: "https://draft.example/v1".into(),
            authentication: ProviderAuthentication::AnthropicApiKey,
            credential_source: DraftCredentialSource::Saved {
                provider_id: "00000000-0000-4000-8000-000000000101".parse().unwrap(),
                provider_revision: 7,
            },
        },
    ];
    for source in sources {
        let operation = muxvia_routing::control::protocol::ControlOperation::DiscoverModels {
            target: muxvia_routing::control::protocol::Target::Codex,
            source,
        };
        let wire = serde_json::to_value(&operation).unwrap();
        assert_eq!(
            serde_json::to_value(
                serde_json::from_value::<muxvia_routing::control::protocol::ControlOperation>(
                    wire.clone()
                )
                .unwrap()
            )
            .unwrap(),
            wire,
        );
        assert!(!format!("{operation:?}").contains("ephemeral-secret-must-not-escape"));
        assert!(!format!("{operation:?}").contains("endpoint-query-must-not-escape"));
    }

    let discovery = ControlResult::ModelDiscovery {
        result: ModelDiscoveryResult::Success {
            models: vec![DiscoveredModel {
                id: "model-a".into(),
                display_name: Some("Owner A".into()),
            }],
            attempts: 1,
            elapsed_ms: 4,
            endpoint_origin: "https://provider.example".into(),
        },
    };
    let reachability = ControlResult::Reachability {
        result: ReachabilityResult::Reachable {
            http_status: 600,
            ttfb_ms: 12,
            checked_at_unix_ms: 1_775_000_000_000,
            retry_count: 0,
            slow: false,
            endpoint_origin: "https://provider.example".into(),
        },
    };
    let maximum_reachability = ControlResult::Reachability {
        result: ReachabilityResult::Reachable {
            http_status: 999,
            ttfb_ms: 12,
            checked_at_unix_ms: 1_775_000_000_000,
            retry_count: 0,
            slow: false,
            endpoint_origin: "https://provider.example".into(),
        },
    };
    for result in [discovery, reachability, maximum_reachability] {
        let wire = serde_json::to_value(&result).unwrap();
        assert!(wire.get("view").is_none());
        assert!(!wire.to_string().contains("targetView"));
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<ControlResult>(wire.clone()).unwrap())
                .unwrap(),
            wire,
        );
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

    let duplicate = TargetAction::DuplicateProvider {
        source_provider_id: "00000000-0000-4000-8000-000000000101".parse().unwrap(),
        source_provider_revision: 7,
        name: "Copied Provider".into(),
        base_url: "https://copied.example/v1".into(),
        model: "copied-model".into(),
        credential: DuplicateCredential::ReuseSource,
    };
    assert_eq!(
        serde_json::to_value(duplicate).unwrap(),
        fixture("duplicate-provider.json")
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
        serde_json::json!({
            "kind": "duplicate-provider",
            "sourceProviderId": "00000000-0000-4000-8000-000000000101",
            "sourceProviderRevision": 0,
            "name": "Copied Provider",
            "baseUrl": "https://copied.example/v1",
            "model": "copied-model",
            "credential": { "kind": "without" },
        }),
    ] {
        assert!(serde_json::from_value::<TargetAction>(action).is_err());
    }
}

#[test]
fn duplicate_credential_debug_redacts_replacement_values() {
    let sentinel = "duplicate-credential-sentinel-must-not-escape";
    let action = TargetAction::DuplicateProvider {
        source_provider_id: "00000000-0000-4000-8000-000000000101".parse().unwrap(),
        source_provider_revision: 1,
        name: "Copied Provider".into(),
        base_url: "https://copied.example/v1".into(),
        model: "copied-model".into(),
        credential: DuplicateCredential::Replace {
            value: sentinel.into(),
        },
    };
    assert!(!format!("{action:?}").contains(sentinel));
}

#[test]
fn provider_declaration_contract_round_trips_the_secret_free_projection_and_actions() {
    let provider = serde_json::json!({
        "id": "00000000-0000-4000-8000-000000000101",
        "position": 0,
        "providerRevision": 1,
        "name": "Direct Provider",
        "baseUrl": "https://provider.example/v1",
        "model": "model-a",
        "protocol": "openai-responses",
        "authentication": "openai-bearer",
        "routingRequirement": "direct-compatible",
        "credential": "present",
        "completeness": "complete",
        "missingFields": [],
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
        "routeHealth": { "state": "unobserved" },
        "providers": [provider.clone()],
        "providerPresets": [{
            "key": "openai-api-responses",
            "baseUrl": "https://api.openai.com/v1",
            "model": "",
            "protocol": "openai-responses",
            "authentication": "openai-bearer"
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
    assert_eq!(
        ProviderRoutingRequirement::DirectCompatible.to_string(),
        "direct-compatible"
    );

    let action = TargetAction::CreateProvider {
        name: "Incomplete".into(),
        base_url: String::new(),
        model: String::new(),
        credential: CredentialEdit::Remove,
        authentication: None,
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
fn claude_declarations_use_the_exact_messages_authentication_and_neutral_health_literals() {
    let claude = serde_json::json!({
        "target": "claude",
        "managementRevision": 0,
        "viewSequence": 0,
        "service": { "epoch": "00000000-0000-4000-8000-000000000001", "state": "running" },
        "mode": "unmanaged",
        "takeover": { "state": "inactive", "endpoint": null },
        "routeHealth": { "state": "unobserved" },
        "providers": [{
            "id": "00000000-0000-4000-8000-000000000101",
            "position": 0,
            "providerRevision": 1,
            "name": "Anthropic API",
            "baseUrl": "https://api.anthropic.com/v1",
            "model": "claude-test",
            "protocol": "anthropic-messages",
            "authentication": "anthropic-api-key",
            "routingRequirement": "takeover-required",
            "credential": "present",
            "completeness": "complete",
            "missingFields": [],
            "provenance": null,
            "generated": false,
            "activeReferences": []
        }],
        "providerPresets": [{
            "key": "anthropic-api-messages",
            "baseUrl": "https://api.anthropic.com/v1",
            "model": "",
            "protocol": "anthropic-messages",
            "authentication": "anthropic-api-key"
        }],
        "currentProviderId": null,
        "servingProviderId": null,
        "managedConfiguration": { "state": "unmanaged", "path": null, "restartRequired": false },
        "recovery": { "intentId": null, "state": "clean" },
        "activatedSnapshot": null,
        "problems": [],
        "futureField": "ignored"
    });
    let parsed: TargetView = serde_json::from_value(claude.clone()).unwrap();
    let serialized = serde_json::to_value(parsed).unwrap();
    assert_eq!(serialized["target"], "claude");
    assert_eq!(serialized["providers"][0]["protocol"], "anthropic-messages");
    assert_eq!(
        serialized["providers"][0]["authentication"],
        "anthropic-api-key"
    );
    assert_eq!(
        serialized["routeHealth"],
        serde_json::json!({ "state": "unobserved" })
    );
    assert!(serialized.get("futureField").is_none());

    for authentication in [
        ProviderAuthentication::AnthropicApiKey,
        ProviderAuthentication::AnthropicBearer,
    ] {
        assert!(
            serde_json::from_value::<ProviderAuthentication>(
                serde_json::to_value(authentication).unwrap()
            )
            .is_ok()
        );
    }

    assert!(has_valid_provider_declaration(
        Target::Codex,
        ProviderProtocol::OpenaiResponses,
        ProviderAuthentication::OpenaiBearer,
    ));
    for invalid in [
        (
            Target::Codex,
            ProviderProtocol::AnthropicMessages,
            ProviderAuthentication::AnthropicApiKey,
        ),
        (
            Target::Claude,
            ProviderProtocol::OpenaiResponses,
            ProviderAuthentication::OpenaiBearer,
        ),
        (
            Target::Claude,
            ProviderProtocol::AnthropicMessages,
            ProviderAuthentication::OpenaiBearer,
        ),
    ] {
        assert!(!has_valid_provider_declaration(
            invalid.0, invalid.1, invalid.2
        ));
    }
}

#[test]
fn activate_provider_uses_exact_direct_and_takeover_wire_modes() {
    for (mode, expected) in [
        ("direct", ActivationMode::Direct),
        ("takeover", ActivationMode::Takeover),
    ] {
        let parsed: TargetAction = serde_json::from_value(serde_json::json!({
            "kind": "activate-provider",
            "providerId": "00000000-0000-4000-8000-000000000101",
            "mode": mode,
            "futureField": "ignored"
        }))
        .unwrap();
        assert_eq!(
            parsed,
            TargetAction::ActivateProvider {
                provider_id: "00000000-0000-4000-8000-000000000101".into(),
                mode: expected,
            }
        );
    }

    assert!(
        serde_json::from_value::<TargetAction>(serde_json::json!({
            "kind": "activate-provider",
            "providerId": "00000000-0000-4000-8000-000000000101",
            "mode": "automatic"
        }))
        .is_err()
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
        "routingRequirement": "takeover-required",
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
        "protocol": "openai-responses",
        "authentication": "openai-bearer",
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
            "protocol": "openai-responses",
            "authentication": "openai-bearer",
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

    let arbitrary_claude_selector = serde_json::json!({
        "type": "request",
        "requestId": "request-2",
        "operation": {
            "kind": "open-target",
            "target": "claude",
            "claudeContext": {
                "claudeConfigDir": null,
                "selectorState": "enabled",
                "blockingSelector": "CREDENTIAL_BEARING_ARBITRARY_VALUE",
                "hostManagedState": "unmanaged",
                "cwd": "/safe/project"
            }
        }
    });
    assert!(serde_json::from_value::<ClientFrame>(arbitrary_claude_selector).is_err());

    for (selector_state, host_managed_state, blocking_selector) in [
        ("enabled", "unmanaged", None),
        ("unknown-nonempty", "unmanaged", None),
        ("unset", "managed", None),
        ("unset", "unmanaged", Some("CLAUDE_CODE_USE_VERTEX")),
    ] {
        let invalid_context = serde_json::json!({
            "type": "request",
            "requestId": "request-context",
            "operation": {
                "kind": "open-target",
                "target": "claude",
                "claudeContext": {
                    "claudeConfigDir": null,
                    "selectorState": selector_state,
                    "blockingSelector": blocking_selector,
                    "hostManagedState": host_managed_state,
                    "cwd": "/safe/project"
                }
            }
        });
        assert!(serde_json::from_value::<ClientFrame>(invalid_context).is_err());
    }

    let arbitrary_problem_selector = serde_json::json!({
        "type": "error",
        "requestId": "request-3",
        "problem": {
            "code": "provider-mode-active",
            "message": "fixed",
            "source": "control-plane-context",
            "selector": "ARBITRARY_SECRET_BEARING_SELECTOR"
        }
    });
    assert!(serde_json::from_value::<ServerFrame>(arbitrary_problem_selector).is_err());

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
