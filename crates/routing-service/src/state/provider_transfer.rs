use secrecy::{ExposeSecret, SecretString};
use std::collections::HashMap;
use subtle::ConstantTimeEq;

use tokio_rusqlite::rusqlite::params;

use crate::control::protocol::{
    ProviderAuthentication, ProviderImportMatchView, ProviderProtocol, ProviderRoutingRequirement,
    Target, TargetView, UniversalProviderCatalogView, UniversalProviderTargetDraft,
};
use crate::domain::view::project_target_view_for;

use super::{StateError, StateStore};

impl StateStore {
    pub(crate) async fn provider_configuration_export_views(
        &self,
    ) -> Result<(UniversalProviderCatalogView, TargetView, TargetView), StateError> {
        let service_epoch = self.service_epoch().to_string();
        self.connection
            .call(move |connection| {
                Ok((
                    super::universal_providers::project_universal_provider_catalog(connection)?,
                    project_target_view_for(connection, &service_epoch, Target::Codex)?,
                    project_target_view_for(connection, &service_epoch, Target::Claude)?,
                ))
            })
            .await
            .map_err(super::store::map_call_error)
    }

    pub(crate) async fn exact_target_provider_import_matches(
        &self,
        target: Target,
        base_url: String,
        model: String,
        protocol: ProviderProtocol,
        authentication: ProviderAuthentication,
        routing_requirement: ProviderRoutingRequirement,
        credential: Option<SecretString>,
    ) -> Result<Vec<ProviderImportMatchView>, StateError> {
        self.connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT p.id, p.name, c.bearer_token
                     FROM providers p
                     LEFT JOIN credentials c ON c.id = p.credential_id
                     WHERE p.target = ?1
                       AND p.base_url = ?2
                       AND p.model = ?3
                       AND p.protocol = ?4
                       AND p.authentication = ?5
                       AND p.routing_requirement = ?6
                       AND p.generated_owner_id IS NULL
                     ORDER BY p.position, p.id",
                )?;
                let rows = statement.query_map(
                    params![
                        target.as_str(),
                        base_url,
                        model,
                        protocol.to_string(),
                        authentication.to_string(),
                        routing_requirement.to_string()
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )?;
                let mut matches = Vec::new();
                for row in rows {
                    let (provider_id, name, existing_credential) = row?;
                    if credentials_equal(credential.as_ref(), existing_credential.as_deref()) {
                        let provider_id = uuid::Uuid::parse_str(&provider_id)
                            .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
                        matches.push(ProviderImportMatchView { provider_id, name });
                    }
                }
                Ok(matches)
            })
            .await
            .map_err(super::store::map_call_error)
    }

    pub(crate) async fn exact_universal_provider_import_matches(
        &self,
        base_url: String,
        targets: Vec<UniversalProviderTargetDraft>,
        credential: Option<SecretString>,
    ) -> Result<Vec<ProviderImportMatchView>, StateError> {
        self.connection
            .call(move |connection| {
                let expected = targets
                    .iter()
                    .map(|overlay| {
                        (
                            overlay.target.as_str().to_owned(),
                            (
                                overlay.enabled,
                                overlay.model.clone(),
                                overlay.authentication.to_string(),
                                overlay.routing_requirement.to_string(),
                            ),
                        )
                    })
                    .collect::<HashMap<_, _>>();
                let mut statement = connection.prepare(
                    "SELECT p.id, p.name, c.bearer_token
                     FROM universal_providers p
                     LEFT JOIN universal_credentials c ON c.id = p.credential_id
                     WHERE p.base_url = ?1
                     ORDER BY p.position, p.id",
                )?;
                let rows = statement.query_map([base_url], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?;
                let mut matches = Vec::new();
                for row in rows {
                    let (provider_id, name, existing_credential) = row?;
                    if !credentials_equal(credential.as_ref(), existing_credential.as_deref()) {
                        continue;
                    }
                    let mut overlay_statement = connection.prepare(
                        "SELECT target, enabled, model, authentication, routing_requirement
                         FROM universal_provider_targets
                         WHERE universal_provider_id = ?1",
                    )?;
                    let overlays = overlay_statement.query_map([&provider_id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            (
                                row.get::<_, bool>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ),
                        ))
                    })?;
                    let existing = overlays.collect::<Result<HashMap<_, _>, _>>()?;
                    if existing == expected {
                        let provider_id = uuid::Uuid::parse_str(&provider_id)
                            .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
                        matches.push(ProviderImportMatchView { provider_id, name });
                    }
                }
                Ok(matches)
            })
            .await
            .map_err(super::store::map_call_error)
    }
}

fn credentials_equal(candidate: Option<&SecretString>, existing: Option<&str>) -> bool {
    match (candidate, existing) {
        (None, None) => true,
        (Some(candidate), Some(existing)) => candidate
            .expose_secret()
            .as_bytes()
            .ct_eq(existing.as_bytes())
            .into(),
        _ => false,
    }
}
