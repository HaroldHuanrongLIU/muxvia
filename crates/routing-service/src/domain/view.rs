use tokio_rusqlite::rusqlite::{Connection, Result};

use crate::control::protocol::{
    ActivatedSnapshotView, ClaudeBlockingSelector, ControlProblem, CredentialPresence,
    ManagedConfigurationView, ProviderAuthentication, ProviderCompleteness,
    ProviderFieldOwnershipView, ProviderProtocol, ProviderProvenanceView, ProviderReferenceView,
    ProviderRequirement, ProviderRoutingRequirement, ProviderView, RecoveryView, RouteHealthView,
    ServiceView, TakeoverView, Target, TargetView, UniversalSynchronizationState,
};
use crate::domain::provider::has_valid_provider_declaration;
use crate::state::providers::provider_presets;

type RouteProjectionRow = (
    u64,
    u64,
    Option<String>,
    Option<String>,
    String,
    Option<i64>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub(crate) fn project_target_view_for(
    connection: &Connection,
    service_epoch: &str,
    target: Target,
) -> Result<TargetView> {
    let target_name = target.as_str();
    let (
        management_revision,
        view_sequence,
        current_provider_id,
        serving_provider_id,
        takeover_state,
        route_port,
        recovery_state,
        managed_config_path,
        activated_snapshot_id,
        recovery_intent_id,
    ): RouteProjectionRow = connection.query_row(
        "SELECT management_revision, view_sequence, current_provider_id, serving_provider_id,
                takeover_state, route_port, recovery_state, managed_config_path,
                activated_snapshot_id, recovery_intent_id
         FROM target_route_state WHERE target = ?1",
        [target_name],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        },
    )?;

    let mut statement = connection.prepare(
        "SELECT p.id, p.position, p.provider_revision, p.name, p.base_url, p.model, p.protocol,
                p.authentication, p.routing_requirement, p.provenance_kind, p.provenance_key,
                p.generated_owner_id, p.generated_source_revision, p.generated_overlay_revision,
                u.provider_revision, t.overlay_revision,
                p.credential_id IS NOT NULL,
                EXISTS(
                    SELECT 1 FROM target_route_state r
                    WHERE r.target = ?1 AND r.current_provider_id = p.id
                ),
                EXISTS(
                    SELECT 1 FROM target_route_state r
                    JOIN activated_snapshots s ON s.id = r.activated_snapshot_id
                    WHERE r.target = ?1 AND s.provider_id = p.id
                )
         FROM providers p
         LEFT JOIN universal_providers u ON u.id = p.generated_owner_id
         LEFT JOIN universal_provider_targets t
           ON t.universal_provider_id = p.generated_owner_id AND t.target = p.target
         WHERE p.target = ?1 ORDER BY p.position",
    )?;
    let providers = statement
        .query_map([target_name], |row| {
            let id = uuid::Uuid::parse_str(&row.get::<_, String>(0)?).map_err(conversion_error)?;
            let has_credential: bool = row.get(16)?;
            let base_url: String = row.get(4)?;
            let model: String = row.get(5)?;
            let mut missing_fields = Vec::new();
            if base_url.is_empty() {
                missing_fields.push(ProviderRequirement::BaseUrl);
            }
            if model.is_empty() {
                missing_fields.push(ProviderRequirement::Model);
            }
            if !has_credential {
                missing_fields.push(ProviderRequirement::Credential);
            }
            let protocol = match row.get::<_, String>(6)?.as_str() {
                "openai-responses" => ProviderProtocol::OpenaiResponses,
                "anthropic-messages" => ProviderProtocol::AnthropicMessages,
                _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
            };
            let authentication = match row.get::<_, String>(7)?.as_str() {
                "openai-bearer" => ProviderAuthentication::OpenaiBearer,
                "anthropic-api-key" => ProviderAuthentication::AnthropicApiKey,
                "anthropic-bearer" => ProviderAuthentication::AnthropicBearer,
                _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
            };
            if !has_valid_provider_declaration(target, protocol, authentication) {
                return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
            }
            let provenance_kind: Option<String> = row.get(9)?;
            let provenance_key: Option<String> = row.get(10)?;
            let provenance = match (provenance_kind, provenance_key) {
                (Some(kind), Some(key)) => Some(ProviderProvenanceView { kind, key }),
                (None, None) => None,
                _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
            };
            let mut active_references = Vec::new();
            if row.get(17)? {
                active_references.push(ProviderReferenceView::Current);
            }
            if row.get(18)? {
                active_references.push(ProviderReferenceView::ActivatedSnapshot);
            }
            let generated_state = match (
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<u64>>(12)?,
                row.get::<_, Option<u64>>(13)?,
                row.get::<_, Option<u64>>(14)?,
                row.get::<_, Option<u64>>(15)?,
            ) {
                (None, None, None, None, None) => None,
                (
                    Some(owner),
                    Some(source),
                    Some(overlay),
                    Some(expected_source),
                    Some(expected_overlay),
                ) => {
                    let owner = uuid::Uuid::parse_str(&owner).map_err(conversion_error)?;
                    Some((
                        owner,
                        if source == expected_source && overlay == expected_overlay {
                            UniversalSynchronizationState::Current
                        } else {
                            UniversalSynchronizationState::Pending
                        },
                    ))
                }
                _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
            };
            Ok(ProviderView {
                id,
                position: row.get(1)?,
                provider_revision: row.get(2)?,
                name: row.get(3)?,
                base_url,
                model,
                protocol,
                authentication,
                routing_requirement: match row.get::<_, String>(8)?.as_str() {
                    "direct-compatible" => ProviderRoutingRequirement::DirectCompatible,
                    "takeover-required" => ProviderRoutingRequirement::TakeoverRequired,
                    _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
                },
                credential: if has_credential {
                    CredentialPresence::Present
                } else {
                    CredentialPresence::Missing
                },
                completeness: if missing_fields.is_empty() {
                    ProviderCompleteness::Complete
                } else {
                    ProviderCompleteness::Incomplete
                },
                missing_fields,
                provenance,
                generated: generated_state.is_some(),
                universal_provider_id: generated_state.map(|value| value.0),
                synchronization: generated_state.map(|value| value.1),
                ownership: if generated_state.is_some() {
                    ProviderFieldOwnershipView::generated()
                } else {
                    ProviderFieldOwnershipView::target_provider()
                },
                active_references,
            })
        })?
        .collect::<Result<Vec<_>>>()?;

    let endpoint = route_port
        .and_then(|port| u16::try_from(port).ok())
        .map(|port| format!("http://127.0.0.1:{port}"));
    let mode = match (takeover_state.as_str(), activated_snapshot_id.as_ref()) {
        ("active", _) => "takeover",
        (_, Some(_)) => "direct",
        _ => "unmanaged",
    };
    let recovery = connection
        .query_row(
            "SELECT id, state FROM activation_recovery
             WHERE target = ?1 AND id = ?2",
            [target_name, recovery_intent_id.as_deref().unwrap_or("")],
            |row| Ok((Some(row.get::<_, String>(0)?), row.get::<_, String>(1)?)),
        )
        .unwrap_or((None, recovery_state.clone()));
    let activated_snapshot = match connection.query_row(
        "SELECT s.id, s.provider_id, s.model, s.protocol, s.authentication, s.epoch
             FROM target_route_state r
             JOIN activated_snapshots s ON s.id = r.activated_snapshot_id
             WHERE r.target = ?1",
        [target_name],
        |row| {
            let id = uuid::Uuid::parse_str(&row.get::<_, String>(0)?).map_err(conversion_error)?;
            let provider_id =
                uuid::Uuid::parse_str(&row.get::<_, String>(1)?).map_err(conversion_error)?;
            let protocol = match row.get::<_, String>(3)?.as_str() {
                "openai-responses" => ProviderProtocol::OpenaiResponses,
                "anthropic-messages" => ProviderProtocol::AnthropicMessages,
                _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
            };
            let authentication = match row.get::<_, String>(4)?.as_str() {
                "openai-bearer" => ProviderAuthentication::OpenaiBearer,
                "anthropic-api-key" => ProviderAuthentication::AnthropicApiKey,
                "anthropic-bearer" => ProviderAuthentication::AnthropicBearer,
                _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
            };
            let epoch =
                uuid::Uuid::parse_str(&row.get::<_, String>(5)?).map_err(conversion_error)?;
            Ok(ActivatedSnapshotView {
                id,
                provider_id,
                model: row.get(2)?,
                protocol,
                authentication,
                epoch,
            })
        },
    ) {
        Ok(snapshot) => Some(snapshot),
        Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => return Err(error),
    };
    let mut problem_statement = connection.prepare(
        "SELECT code, message, source, selector
         FROM target_problems WHERE target = ?1 ORDER BY code",
    )?;
    let problems = problem_statement
        .query_map([target_name], |row| {
            let selector = row
                .get::<_, Option<String>>(3)?
                .map(|value| {
                    ClaudeBlockingSelector::from_str(&value)
                        .ok_or(tokio_rusqlite::rusqlite::Error::InvalidQuery)
                })
                .transpose()?;
            Ok(ControlProblem {
                code: row.get(0)?,
                message: row.get(1)?,
                source: row.get(2)?,
                selector,
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    let configuration_drift = problems
        .iter()
        .any(|problem| problem.code == "configuration-drift");
    let startup_unavailable = problems.iter().any(|problem| {
        matches!(
            problem.code.as_str(),
            "startup-reconciliation-failed" | "model-route-unavailable"
        )
    });

    Ok(TargetView {
        target,
        management_revision,
        view_sequence,
        service: ServiceView {
            epoch: service_epoch.to_owned(),
            state: "running".to_owned(),
        },
        mode: mode.to_owned(),
        takeover: TakeoverView {
            state: if startup_unavailable {
                "unavailable".to_owned()
            } else {
                takeover_state.clone()
            },
            endpoint: (!startup_unavailable).then_some(endpoint).flatten(),
        },
        route_health: RouteHealthView {
            state: "unobserved".to_owned(),
        },
        providers,
        provider_presets: provider_presets(target),
        current_provider_id,
        serving_provider_id,
        managed_configuration: ManagedConfigurationView {
            state: if recovery_state == "recovery-required" {
                recovery_state.clone()
            } else if configuration_drift {
                "configuration-drift".to_owned()
            } else if mode != "unmanaged" {
                "applied".to_owned()
            } else {
                "unmanaged".to_owned()
            },
            path: managed_config_path,
            restart_required: mode != "unmanaged",
        },
        recovery: RecoveryView {
            intent_id: recovery.0,
            state: if recovery_state == "recovery-required" {
                recovery_state
            } else {
                recovery.1
            },
        },
        activated_snapshot,
        problems,
    })
}

fn conversion_error(
    error: impl std::error::Error + Send + Sync + 'static,
) -> tokio_rusqlite::rusqlite::Error {
    tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
        0,
        tokio_rusqlite::rusqlite::types::Type::Text,
        Box::new(error),
    )
}

pub(crate) fn empty_target_view_for(service_epoch: &str, target: Target) -> TargetView {
    TargetView {
        target,
        management_revision: 0,
        view_sequence: 0,
        service: ServiceView {
            epoch: service_epoch.to_owned(),
            state: "running".to_owned(),
        },
        mode: "unmanaged".to_owned(),
        takeover: TakeoverView {
            state: "inactive".to_owned(),
            endpoint: None,
        },
        route_health: RouteHealthView {
            state: "unobserved".to_owned(),
        },
        providers: Vec::new(),
        provider_presets: provider_presets(target),
        current_provider_id: None,
        serving_provider_id: None,
        managed_configuration: ManagedConfigurationView {
            state: "unmanaged".to_owned(),
            path: None,
            restart_required: false,
        },
        recovery: RecoveryView {
            intent_id: None,
            state: "clean".to_owned(),
        },
        activated_snapshot: None,
        problems: Vec::new(),
    }
}
