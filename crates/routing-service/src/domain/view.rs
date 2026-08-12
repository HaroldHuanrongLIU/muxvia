use tokio_rusqlite::rusqlite::{Connection, Result};

use crate::control::protocol::{
    CredentialPresence, ManagedConfigurationView, ProviderView, ServiceView, TakeoverView, Target,
    TargetView,
};

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
    ): (u64, u64, Option<String>, Option<String>, String, Option<u16>) = connection.query_row(
        "SELECT management_revision, view_sequence, current_provider_id, serving_provider_id, takeover_state, route_port
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
        "direct"
    };

    Ok(TargetView {
        target: Target::Codex,
        management_revision,
        view_sequence,
        service: ServiceView {
            epoch: service_epoch.to_owned(),
            state: "ready".to_owned(),
        },
        mode: mode.to_owned(),
        takeover: TakeoverView {
            state: takeover_state,
            endpoint,
        },
        providers,
        current_provider_id,
        serving_provider_id,
        managed_configuration: ManagedConfigurationView {
            state: "unmanaged".to_owned(),
            path: None,
            restart_required: false,
        },
        activated_snapshot: None,
        problems: Vec::new(),
    })
}

pub(crate) fn empty_target_view(service_epoch: &str) -> TargetView {
    TargetView {
        target: Target::Codex,
        management_revision: 0,
        view_sequence: 0,
        service: ServiceView {
            epoch: service_epoch.to_owned(),
            state: "ready".to_owned(),
        },
        mode: "direct".to_owned(),
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
        activated_snapshot: None,
        problems: Vec::new(),
    }
}
