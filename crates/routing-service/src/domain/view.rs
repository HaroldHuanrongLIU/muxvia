use tokio_rusqlite::rusqlite::{Connection, Result};

use crate::control::protocol::{
    ActivatedSnapshotView, CredentialPresence, ManagedConfigurationView, ProviderView,
    RecoveryView, ServiceView, TakeoverView, Target, TargetView,
};

type RouteProjectionRow = (
    u64,
    u64,
    Option<String>,
    Option<String>,
    String,
    Option<u16>,
    String,
    Option<String>,
);

pub(crate) fn project_target_view(
    connection: &Connection,
    service_epoch: &str,
) -> Result<TargetView> {
    let (
        management_revision,
        view_sequence,
        current_provider_id,
        serving_provider_id,
        takeover_state,
        route_port,
        recovery_state,
        managed_config_path,
    ): RouteProjectionRow = connection.query_row(
        "SELECT management_revision, view_sequence, current_provider_id, serving_provider_id, takeover_state, route_port, recovery_state, managed_config_path
         FROM target_route_state WHERE target = 'codex'",
        [],
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
            ))
        },
    )?;

    let mut statement = connection.prepare(
        "SELECT p.id, p.name, p.base_url, p.model,
                EXISTS(SELECT 1 FROM provider_credentials c WHERE c.provider_id = p.id)
         FROM providers p WHERE p.target = 'codex' ORDER BY p.rowid",
    )?;
    let providers = statement
        .query_map([], |row| {
            let has_credential: bool = row.get(4)?;
            Ok(ProviderView {
                id: row.get(0)?,
                name: row.get(1)?,
                base_url: row.get(2)?,
                model: row.get(3)?,
                credential: if has_credential {
                    CredentialPresence::Present
                } else {
                    CredentialPresence::Missing
                },
            })
        })?
        .collect::<Result<Vec<_>>>()?;

    let endpoint = route_port.map(|port| format!("http://127.0.0.1:{port}"));
    let mode = if takeover_state == "active" {
        "takeover"
    } else {
        "unmanaged"
    };
    let recovery = connection
        .query_row(
            "SELECT id, state FROM activation_recovery ORDER BY rowid DESC LIMIT 1",
            [],
            |row| Ok((Some(row.get::<_, String>(0)?), row.get::<_, String>(1)?)),
        )
        .unwrap_or((None, recovery_state.clone()));
    let activated_snapshot = match connection.query_row(
        "SELECT s.id, s.provider_id, s.model, s.epoch
             FROM target_route_state r
             JOIN activated_snapshots s ON s.id = r.activated_snapshot_id
             WHERE r.target = 'codex'",
        [],
        |row| {
            let id = uuid::Uuid::parse_str(&row.get::<_, String>(0)?).map_err(conversion_error)?;
            let provider_id =
                uuid::Uuid::parse_str(&row.get::<_, String>(1)?).map_err(conversion_error)?;
            let epoch =
                uuid::Uuid::parse_str(&row.get::<_, String>(3)?).map_err(conversion_error)?;
            Ok(ActivatedSnapshotView {
                id,
                provider_id,
                model: row.get(2)?,
                epoch,
            })
        },
    ) {
        Ok(snapshot) => Some(snapshot),
        Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => return Err(error),
    };

    Ok(TargetView {
        target: Target::Codex,
        management_revision,
        view_sequence,
        service: ServiceView {
            epoch: service_epoch.to_owned(),
            state: "running".to_owned(),
        },
        mode: mode.to_owned(),
        takeover: TakeoverView {
            state: takeover_state.clone(),
            endpoint,
        },
        providers,
        current_provider_id,
        serving_provider_id,
        managed_configuration: ManagedConfigurationView {
            state: if recovery_state == "recovery-required" {
                recovery_state.clone()
            } else if takeover_state == "active" {
                "applied".to_owned()
            } else {
                "unmanaged".to_owned()
            },
            path: managed_config_path,
            restart_required: takeover_state == "active",
        },
        recovery: RecoveryView {
            intent_id: recovery.0,
            state: recovery.1,
        },
        activated_snapshot,
        problems: Vec::new(),
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

pub(crate) fn empty_target_view(service_epoch: &str) -> TargetView {
    TargetView {
        target: Target::Codex,
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
        providers: Vec::new(),
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
