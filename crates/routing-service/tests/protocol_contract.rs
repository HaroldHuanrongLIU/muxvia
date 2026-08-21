use std::path::PathBuf;

use muxvia_routing::{
    control::{
        framing::{FrameError, read_frame, write_frame},
        protocol::{
            ActivationMode, ClientFrame, ControlResult, CredentialEdit, DiscoverySource,
            DraftCredentialSource, DuplicateCredential, ProviderAuthentication,
            ProviderCompleteness, ProviderConfigurationExport, ProviderImportPreview,
            ProviderProtocol, ProviderRequirement, ProviderRoutingRequirement, ServerFrame,
            SubscriptionAccountAction, Target, TargetAction, TargetView, UniversalProviderAction,
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

    let failover_view = fixture("failover-target-view.json");
    let view: TargetView = serde_json::from_value(failover_view.clone()).unwrap();
    assert_eq!(serde_json::to_value(view).unwrap(), failover_view);

    let save_provider = fixture("save-provider.json");
    let action: TargetAction = serde_json::from_value(save_provider.clone()).unwrap();
    assert_eq!(serde_json::to_value(action).unwrap(), save_provider);

    let subscription_bridge = fixture("create-subscription-bridge-provider.json");
    let action: TargetAction = serde_json::from_value(subscription_bridge.clone()).unwrap();
    assert_eq!(serde_json::to_value(action).unwrap(), subscription_bridge);

    for name in [
        "reorder-providers.json",
        "delete-provider.json",
        "duplicate-provider.json",
        "save-failover-draft.json",
        "apply-failover-chain.json",
        "disable-takeover.json",
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
        "list-request-records.json",
        "inspect-request-record.json",
        "list-usage-activity.json",
        "refresh-native-usage.json",
        "set-usage-retention.json",
        "clear-usage.json",
        "update-pricing-catalog.json",
        "open-universal-providers.json",
        "prepare-handover.json",
        "preview-provider-import.json",
        "preview-cc-switch-sql-import.json",
        "confirm-provider-import.json",
        "confirm-cc-switch-sql-import.json",
        "export-provider-configuration.json",
        "create-recovery-backup.json",
        "inspect-recovery-backup.json",
        "restore-recovery-backup.json",
        "force-stop.json",
    ] {
        let frame = fixture(name);
        let parsed: ClientFrame = serde_json::from_value(frame.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), frame);
    }

    let compatibility_probe = fixture("compatibility-probe.json");
    let parsed: ServerFrame = serde_json::from_value(compatibility_probe.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), compatibility_probe);

    let handover_prepared = fixture("handover-prepared.json");
    let parsed: ServerFrame = serde_json::from_value(handover_prepared.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), handover_prepared);

    let force_stop_accepted = fixture("force-stop-accepted.json");
    let parsed: ServerFrame = serde_json::from_value(force_stop_accepted.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), force_stop_accepted);

    for name in ["request-record-page.json", "request-record-detail.json"] {
        let frame = fixture(name);
        let parsed: ServerFrame = serde_json::from_value(frame.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), frame);
    }

    for name in [
        "usage-activity-page.json",
        "migrated-usage-activity-page.json",
        "native-usage-refresh.json",
        "usage-retention-outcome.json",
        "usage-clear-outcome.json",
        "pricing-catalog-update-outcome.json",
        "provider-import-preview.json",
        "cc-switch-sql-import-preview.json",
        "provider-import-outcome.json",
        "cc-switch-sql-import-outcome.json",
        "provider-configuration-export.json",
        "recovery-backup-created.json",
        "recovery-backup-inspection.json",
    ] {
        let frame = fixture(name);
        let parsed: ServerFrame = serde_json::from_value(frame.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), frame);
    }

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

    let account_open = fixture("open-subscription-accounts.json");
    let parsed: ClientFrame = serde_json::from_value(account_open.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), account_open);

    let account_catalog = fixture("subscription-account-catalog.json");
    let parsed: ServerFrame = serde_json::from_value(account_catalog.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), account_catalog);

    let account_action = fixture("set-default-subscription-account.json");
    let parsed: SubscriptionAccountAction = serde_json::from_value(account_action.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), account_action);

    for name in [
        "start-device-authorization.json",
        "poll-device-authorization.json",
        "preview-default-subscription-account.json",
        "subscription-account-act.json",
    ] {
        let frame = fixture(name);
        let parsed: ClientFrame = serde_json::from_value(frame.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), frame);
    }
    for name in [
        "device-authorization-challenge.json",
        "device-authorization-poll.json",
        "subscription-default-preview.json",
        "subscription-account-outcome.json",
    ] {
        let frame = fixture(name);
        let parsed: ServerFrame = serde_json::from_value(frame.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), frame);
    }
}

#[test]
fn recovery_backup_contract_is_closed_sensitive_and_secret_free() {
    let inspection_request = fixture("inspect-recovery-backup.json");
    let parsed: ClientFrame = serde_json::from_value(inspection_request.clone()).unwrap();
    assert!(
        !format!("{parsed:?}").contains("00000000-0000-4000-8000-000000000171"),
        "Recovery Backup request Debug exposed the selected path"
    );
    let restore_request = fixture("restore-recovery-backup.json");
    assert!(
        serde_json::from_value::<ClientFrame>(restore_request).is_ok(),
        "closed restore request fixture must round-trip"
    );
    let restored = fixture("recovery-backup-restored.json");
    assert!(
        serde_json::from_value::<ServerFrame>(restored).is_ok(),
        "closed restore result fixture must round-trip"
    );
    for (name, branch) in [
        ("create-recovery-backup.json", "operation"),
        ("inspect-recovery-backup.json", "operation"),
        ("restore-recovery-backup.json", "operation"),
        ("recovery-backup-created.json", "result"),
        ("recovery-backup-inspection.json", "result"),
        ("recovery-backup-restored.json", "result"),
    ] {
        let mut value = fixture(name);
        value[branch]["credential"] = serde_json::json!("RECOVERY_BACKUP_PROTOCOL_SECRET_17001");
        let rejected = if branch == "operation" {
            serde_json::from_value::<ClientFrame>(value).is_err()
        } else {
            serde_json::from_value::<ServerFrame>(value).is_err()
        };
        assert!(rejected, "accepted additive secret field in {name}");
    }
    let schema = fixture("../control-v1.schema.json");
    assert_eq!(
        schema["$defs"]["recoveryBackupInspection"]["additionalProperties"],
        false
    );
    assert_eq!(
        schema["$defs"]["recoveryBackupEntry"]["additionalProperties"],
        false
    );
}

#[test]
fn usage_lifecycle_contract_is_closed_target_bound_and_secret_free() {
    let client_fixtures = [
        "list-usage-activity.json",
        "refresh-native-usage.json",
        "set-usage-retention.json",
        "clear-usage.json",
        "update-pricing-catalog.json",
    ];
    let server_fixtures = [
        "usage-activity-page.json",
        "migrated-usage-activity-page.json",
        "native-usage-refresh.json",
        "usage-retention-outcome.json",
        "usage-clear-outcome.json",
        "pricing-catalog-update-outcome.json",
    ];
    for name in client_fixtures {
        let mut frame = fixture(name);
        frame["operation"]["nativeContent"] =
            serde_json::json!("NATIVE_USAGE_PROTOCOL_SECRET_14001");
        assert!(
            serde_json::from_value::<ClientFrame>(frame).is_err(),
            "accepted additive native usage content in {name}"
        );
    }
    for name in server_fixtures {
        let mut frame = fixture(name);
        frame["result"]["sourcePath"] = serde_json::json!("NATIVE_USAGE_PROTOCOL_SECRET_14002");
        assert!(
            serde_json::from_value::<ServerFrame>(frame).is_err(),
            "accepted additive native source path in {name}"
        );
    }

    let schema = fixture("../control-v1.schema.json");
    for definition in [
        "nativeUsageRecordSummary",
        "dailyUsageRollup",
        "migratedUsageRollup",
        "usageActivityEntry",
        "usageActivityPage",
        "nativeUsageRefresh",
        "usageRetentionOutcome",
        "usageClearOutcome",
        "pricingCatalogUpdateOutcome",
    ] {
        assert_eq!(schema["$defs"][definition]["additionalProperties"], false);
    }
}

#[test]
fn provider_transfer_contract_is_preview_first_closed_and_secret_free() {
    let request = fixture("preview-provider-import.json");
    let parsed: ClientFrame = serde_json::from_value(request.clone()).unwrap();
    assert!(
        !format!("{parsed:?}").contains("provider-import-secret-must-not-escape"),
        "preview request Debug rendered pasted Provider credentials"
    );

    let preview = fixture("provider-import-preview.json");
    let parsed: ServerFrame = serde_json::from_value(preview.clone()).unwrap();
    let serialized = serde_json::to_string(&parsed).unwrap();
    assert!(!serialized.contains("provider-import-secret-must-not-escape"));
    assert!(!format!("{parsed:?}").contains("provider-import-secret-must-not-escape"));

    let sql_request = fixture("preview-cc-switch-sql-import.json");
    let parsed: ClientFrame = serde_json::from_value(sql_request).unwrap();
    assert!(
        !format!("{parsed:?}").contains("cc-switch-export.sql"),
        "SQL import request Debug rendered the Operator-selected path"
    );

    let export = fixture("provider-configuration-export.json");
    let parsed: ServerFrame = serde_json::from_value(export.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), export);

    for (name, branch) in [
        ("preview-provider-import.json", "operation"),
        ("preview-cc-switch-sql-import.json", "operation"),
        ("confirm-provider-import.json", "operation"),
        ("confirm-cc-switch-sql-import.json", "operation"),
        ("export-provider-configuration.json", "operation"),
        ("provider-import-preview.json", "result"),
        ("cc-switch-sql-import-preview.json", "result"),
        ("provider-import-outcome.json", "result"),
        ("cc-switch-sql-import-outcome.json", "result"),
        ("provider-configuration-export.json", "result"),
    ] {
        let mut value = fixture(name);
        value[branch]["additiveSecret"] =
            serde_json::json!("PROVIDER_TRANSFER_ADDITIVE_SECRET_16001");
        let rejected = if branch == "operation" {
            serde_json::from_value::<ClientFrame>(value).is_err()
        } else {
            serde_json::from_value::<ServerFrame>(value).is_err()
        };
        assert!(rejected, "accepted additive field in {name}");
    }

    let result =
        match serde_json::from_value::<ServerFrame>(fixture("provider-import-preview.json"))
            .unwrap()
        {
            ServerFrame::Response {
                result: ControlResult::ProviderImportPreview(result),
                ..
            } => result.preview,
            _ => panic!("unexpected preview fixture"),
        };
    let _: ProviderImportPreview = result;

    let exported =
        match serde_json::from_value::<ServerFrame>(fixture("provider-configuration-export.json"))
            .unwrap()
        {
            ServerFrame::Response {
                result: ControlResult::ProviderConfigurationExport(result),
                ..
            } => result.export,
            _ => panic!("unexpected export fixture"),
        };
    let _: ProviderConfigurationExport = exported;

    let schema = fixture("../control-v1.schema.json");
    let branch = |definition: &str, discriminator: &str| {
        schema["$defs"][definition]["oneOf"]
            .as_array()
            .and_then(|branches| {
                branches
                    .iter()
                    .find(|branch| branch["properties"]["kind"]["const"] == discriminator)
            })
            .expect("missing Provider Transfer schema branch")
    };
    for (definition, discriminator) in [
        ("controlOperation", "preview-provider-import"),
        ("controlOperation", "confirm-provider-import"),
        ("controlOperation", "export-provider-configuration"),
        ("controlResult", "provider-import-preview"),
        ("controlResult", "provider-import-outcome"),
        ("controlResult", "provider-configuration-export"),
    ] {
        assert_eq!(
            branch(definition, discriminator)["additionalProperties"],
            false
        );
    }
    for definition in [
        "providerImportPreview",
        "providerImportHistoricalUsagePreview",
        "providerImportTargetCandidate",
        "providerImportUniversalCandidate",
        "providerImportChoice",
        "providerConfigurationExport",
        "exportedTargetProvider",
        "exportedUniversalProvider",
        "exportedFailoverDraft",
    ] {
        assert_eq!(schema["$defs"][definition]["additionalProperties"], false);
    }
}

#[test]
fn request_history_contract_is_closed_target_bound_and_payload_bounded() {
    let list = fixture("list-request-records.json");
    let mut detail = fixture("request-record-detail.json");
    detail["result"]["detail"]["errorPayload"] =
        serde_json::json!("REQUEST_HISTORY_DEBUG_SECRET_13102");
    assert!(serde_json::from_value::<ClientFrame>(list.clone()).is_ok());
    assert!(serde_json::from_value::<ServerFrame>(detail.clone()).is_ok());
    let parsed: ServerFrame = serde_json::from_value(detail.clone()).unwrap();
    assert!(
        !format!("{parsed:?}").contains("REQUEST_HISTORY_DEBUG_SECRET_13102"),
        "request-history Debug rendered a failed upstream payload"
    );

    for (mut value, branch) in [(list.clone(), "operation"), (detail.clone(), "result")] {
        value[branch]["credential"] = serde_json::json!("REQUEST_HISTORY_PROTOCOL_SECRET_13101");
        let rejected = if branch == "operation" {
            serde_json::from_value::<ClientFrame>(value).is_err()
        } else {
            serde_json::from_value::<ServerFrame>(value).is_err()
        };
        assert!(rejected, "accepted additive request-history credential");
    }

    let mut over_limit = list.clone();
    over_limit["operation"]["limit"] = serde_json::json!(101);
    assert!(serde_json::from_value::<ClientFrame>(over_limit).is_err());

    let mut zero_limit = list;
    zero_limit["operation"]["limit"] = serde_json::json!(0);
    assert!(serde_json::from_value::<ClientFrame>(zero_limit).is_err());

    let mut wrong_target = detail;
    wrong_target["result"]["detail"]["target"] = serde_json::json!("claude");
    let parsed: ServerFrame = serde_json::from_value(wrong_target).unwrap();
    assert!(matches!(
        parsed,
        ServerFrame::Response {
            result: ControlResult::RequestRecordDetail(result),
            ..
        } if result.detail.target == Target::Claude
    ));

    let schema = fixture("../control-v1.schema.json");
    let branch = |definition: &str, discriminator: &str| {
        schema["$defs"][definition]["oneOf"]
            .as_array()
            .and_then(|branches| {
                branches
                    .iter()
                    .find(|branch| branch["properties"]["kind"]["const"] == discriminator)
            })
            .expect("missing request-history schema branch")
    };
    for (definition, discriminator) in [
        ("controlOperation", "list-request-records"),
        ("controlOperation", "inspect-request-record"),
        ("controlResult", "request-record-page"),
        ("controlResult", "request-record-detail"),
    ] {
        assert_eq!(
            branch(definition, discriminator)["additionalProperties"],
            false
        );
    }
    assert_eq!(
        schema["$defs"]["requestRecordOutcome"]["enum"],
        serde_json::json!([
            "success",
            "upstream-error",
            "semantic-error",
            "transport-error",
            "route-unavailable",
            "cancelled",
            "stream-error"
        ])
    );
    for definition in [
        "requestUsage",
        "requestRecordSummary",
        "pricingSnapshot",
        "requestRecordPage",
        "requestRecordDetail",
    ] {
        assert_eq!(schema["$defs"][definition]["additionalProperties"], false);
    }
}

#[test]
fn subscription_bridge_provider_contract_is_closed_and_credentialless() {
    let action = fixture("create-subscription-bridge-provider.json");
    let parsed: TargetAction = serde_json::from_value(action.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), action);

    let mut credential = action.clone();
    credential["credential"] = serde_json::json!({
        "kind": "replace",
        "value": "SUBSCRIPTION_BRIDGE_PROTOCOL_SECRET_12801"
    });
    assert!(serde_json::from_value::<TargetAction>(credential).is_ok());

    let mut additive = action;
    additive["accessToken"] = serde_json::json!("SUBSCRIPTION_BRIDGE_PROTOCOL_SECRET_12802");
    let parsed: TargetAction = serde_json::from_value(additive).unwrap();
    assert!(
        !serde_json::to_string(&parsed)
            .unwrap()
            .contains("SUBSCRIPTION_BRIDGE_PROTOCOL_SECRET_12802"),
        "additive subscription secret survived the typed action boundary"
    );
}

#[test]
fn subscription_account_contract_is_closed_and_secret_free() {
    let mut action = fixture("set-default-subscription-account.json");
    action["refreshToken"] = serde_json::json!("SUBSCRIPTION_PROTOCOL_SECRET_11701");
    assert!(serde_json::from_value::<SubscriptionAccountAction>(action).is_err());

    let mut catalog = fixture("subscription-account-catalog.json");
    catalog["result"]["view"]["accounts"][0]["refreshToken"] =
        serde_json::json!("SUBSCRIPTION_PROTOCOL_SECRET_11701");
    assert!(serde_json::from_value::<ServerFrame>(catalog).is_err());
}

#[test]
fn lifecycle_contracts_are_closed_and_secret_free() {
    let disabled = fixture("disable-takeover.json");
    let prepared = fixture("prepare-handover.json");
    let accepted = fixture("handover-prepared.json");
    let forced = fixture("force-stop.json");
    let force_accepted = fixture("force-stop-accepted.json");

    assert!(serde_json::from_value::<TargetAction>(disabled.clone()).is_ok());
    assert!(serde_json::from_value::<ClientFrame>(prepared.clone()).is_ok());
    assert!(serde_json::from_value::<ServerFrame>(accepted.clone()).is_ok());
    assert!(serde_json::from_value::<ClientFrame>(forced.clone()).is_ok());
    assert!(serde_json::from_value::<ServerFrame>(force_accepted.clone()).is_ok());
    let mut wrong_acknowledgement = forced.clone();
    wrong_acknowledgement["operation"]["acknowledgement"] = serde_json::json!("yes");
    assert!(serde_json::from_value::<ClientFrame>(wrong_acknowledgement).is_err());
    let mut wrong_warning = force_accepted.clone();
    wrong_warning["result"]["warning"] = serde_json::json!("yes");
    assert!(serde_json::from_value::<ServerFrame>(wrong_warning).is_err());

    for (mut value, label) in [
        (disabled, "disable"),
        (prepared, "prepare"),
        (accepted, "accepted"),
        (forced, "forced"),
        (force_accepted, "force-accepted"),
    ] {
        let object = if label == "disable" {
            value.as_object_mut().unwrap()
        } else if label == "prepare" || label == "forced" {
            value["operation"].as_object_mut().unwrap()
        } else {
            value["result"].as_object_mut().unwrap()
        };
        object.insert(
            "additiveSecret".to_owned(),
            serde_json::json!("LIFECYCLE_PROTOCOL_SECRET_40391"),
        );
        let rejected = match label {
            "disable" => serde_json::from_value::<TargetAction>(value).is_err(),
            "prepare" | "forced" => serde_json::from_value::<ClientFrame>(value).is_err(),
            _ => serde_json::from_value::<ServerFrame>(value).is_err(),
        };
        assert!(rejected, "accepted additive lifecycle field");
    }
}

#[test]
fn failover_actions_are_closed_and_revision_bound() {
    let save = fixture("save-failover-draft.json");
    let apply = fixture("apply-failover-chain.json");

    let saved: TargetAction = serde_json::from_value(save.clone()).unwrap();
    let applied: TargetAction = serde_json::from_value(apply.clone()).unwrap();
    assert_eq!(serde_json::to_value(saved).unwrap(), save);
    assert_eq!(serde_json::to_value(applied).unwrap(), apply);

    for (fixture_name, field) in [
        ("save-failover-draft.json", "additiveSecret"),
        ("apply-failover-chain.json", "additiveSecret"),
    ] {
        let mut invalid = fixture(fixture_name);
        invalid[field] = serde_json::json!("FAILOVER_PROTOCOL_SECRET_86421");
        assert!(
            serde_json::from_value::<TargetAction>(invalid).is_err(),
            "accepted additive failover action field"
        );
    }

    let mut invalid_member_revision = fixture("save-failover-draft.json");
    invalid_member_revision["members"][0]["providerRevision"] = serde_json::json!(0);
    assert!(serde_json::from_value::<TargetAction>(invalid_member_revision).is_err());

    let mut invalid_draft_revision = fixture("apply-failover-chain.json");
    invalid_draft_revision["draftRevision"] = serde_json::json!(0);
    assert!(serde_json::from_value::<TargetAction>(invalid_draft_revision).is_err());

    let schema = fixture("../control-v1.schema.json");
    let branches = schema["$defs"]["targetAction"]["oneOf"]
        .as_array()
        .expect("targetAction branches");
    for discriminator in ["save-failover-draft", "apply-failover-chain"] {
        let branch = branches
            .iter()
            .find(|branch| branch["properties"]["kind"]["const"] == discriminator)
            .expect("missing failover action schema branch");
        assert_eq!(branch["additionalProperties"], false);
    }
}

#[test]
fn failover_view_schema_is_closed_and_complete() {
    let schema = fixture("../control-v1.schema.json");
    let target = &schema["$defs"]["targetView"];
    assert_eq!(
        target["properties"]["failover"]["$ref"],
        "#/$defs/failoverView"
    );
    assert!(
        target["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("failover"))
    );
    assert_eq!(
        schema["$defs"]["provider"]["properties"]["routeHealth"]["$ref"],
        "#/$defs/routeHealth"
    );
    assert!(
        schema["$defs"]["provider"]["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("routeHealth"))
    );
    assert_eq!(
        schema["$defs"]["routeHealth"]["properties"]["state"]["enum"],
        serde_json::json!(["unobserved", "healthy", "degraded", "unavailable", "stale"])
    );
    assert_eq!(
        schema["$defs"]["failoverView"]["additionalProperties"],
        false
    );
    assert_eq!(
        schema["$defs"]["activatedRoutePlan"]["additionalProperties"],
        false
    );
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
        schema["$defs"]["universalProviderPresetTarget"]["properties"]["authentication"]["enum"]
            .as_array()
            .is_some_and(|values| { values.iter().all(|value| value != "codex-subscription") }),
        "Subscription Bridge widened Universal Provider authentication"
    );
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
    let update = TargetAction::UpdateProvider {
        provider_id: "00000000-0000-4000-8000-000000000101".into(),
        provider_revision: 7,
        name: "Overlay Provider".into(),
        base_url: "https://overlay.example/v1".into(),
        model: "overlay-model".into(),
        credential: CredentialEdit::Keep,
        authentication: Some(ProviderAuthentication::OpenaiBearer),
        routing_requirement: Some(ProviderRoutingRequirement::TakeoverRequired),
    };
    assert_eq!(
        serde_json::to_value(update).unwrap(),
        serde_json::json!({
            "kind": "update-provider",
            "providerId": "00000000-0000-4000-8000-000000000101",
            "providerRevision": 7,
            "name": "Overlay Provider",
            "baseUrl": "https://overlay.example/v1",
            "model": "overlay-model",
            "credential": { "kind": "keep" },
            "authentication": "openai-bearer",
            "routingRequirement": "takeover-required"
        })
    );

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
        "universalProviderId": null,
        "synchronization": null,
        "ownership": {
            "name": "target-provider",
            "baseUrl": "target-provider",
            "model": "target-provider",
            "protocol": "target-fixed",
            "authentication": "target-provider",
            "routingRequirement": "target-provider",
            "credential": "target-provider"
        },
        "routeHealth": { "state": "unobserved" },
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
        "failover": { "draftRevision": 1, "draftMembers": [], "activePlan": null },
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
            "universalProviderId": null,
            "synchronization": null,
            "ownership": {
                "name": "target-provider",
                "baseUrl": "target-provider",
                "model": "target-provider",
                "protocol": "target-fixed",
                "authentication": "target-provider",
                "routingRequirement": "target-provider",
                "credential": "target-provider"
            },
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
    assert_eq!(
        serialized["providers"][0]["routeHealth"],
        serde_json::json!({ "state": "unobserved" })
    );
    assert_eq!(
        serialized["failover"],
        serde_json::json!({
            "draftRevision": 0,
            "draftMembers": [],
            "activePlan": null
        })
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
