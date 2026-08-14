use serde::Deserialize;
use tokio_rusqlite::rusqlite::{
    Connection, Error, OptionalExtension, Result, Transaction, TransactionBehavior, params,
    types::Type,
};

use crate::control::protocol::{
    ActionOutcome, ActionStatus, ActivatedSnapshotView, ControlProblem, CredentialPresence,
    ManagedConfigurationView, ProviderCompleteness, ProviderProtocol, ProviderReferenceView,
    ProviderRequirement, ProviderView, RecoveryView, ServiceView, TakeoverView, Target, TargetView,
};

const SCHEMA: &str = include_str!("schema.sql");

pub const SCHEMA_VERSION: u32 = 2;

pub fn migrate(connection: &mut Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS metadata (
           key TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );",
    )?;
    let version = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema-version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)
        })
        .transpose()?;

    match version {
        None | Some(SCHEMA_VERSION) => connection.execute_batch(SCHEMA)?,
        Some(1) => migrate_v1(connection)?,
        Some(_) => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
    }
    Ok(())
}

fn migrate_v1(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE credentials (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target = 'codex'),
           bearer_token TEXT NOT NULL
         );
         CREATE TABLE providers_v2 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target = 'codex'),
           position INTEGER NOT NULL CHECK (position >= 0),
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           name TEXT NOT NULL,
           base_url TEXT NOT NULL,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol = 'openai-responses'),
           credential_id TEXT REFERENCES credentials(id) ON DELETE SET NULL,
           provenance_kind TEXT,
           provenance_key TEXT,
           generated_owner_id TEXT
         );",
    )?;

    let providers = {
        let mut statement = transaction.prepare(
            "SELECT p.id, p.target, p.name, p.base_url, p.model, c.bearer_token
             FROM providers p
             LEFT JOIN provider_credentials c ON c.provider_id = p.id
             ORDER BY p.rowid",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?
    };
    for (position, (id, target, name, base_url, model, credential)) in
        providers.into_iter().enumerate()
    {
        let credential_id = credential.as_ref().map(|_| id.as_str());
        if let Some(credential) = credential {
            transaction.execute(
                "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, ?2, ?3)",
                params![id, target, credential],
            )?;
        }
        transaction.execute(
            "INSERT INTO providers_v2
             (id, target, position, provider_revision, name, base_url, model, protocol, credential_id,
              provenance_kind, provenance_key, generated_owner_id)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, 'openai-responses', ?7, NULL, NULL, NULL)",
            params![id, target, position as u32, name, base_url, model, credential_id],
        )?;
    }
    migrate_v1_receipts(&transaction)?;
    transaction.execute_batch(
        "DROP TABLE provider_credentials;
         DROP TABLE providers;
         ALTER TABLE providers_v2 RENAME TO providers;
         UPDATE metadata SET value = '2' WHERE key = 'schema-version';",
    )?;
    transaction.commit()?;
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

fn migrate_v1_receipts(transaction: &Transaction<'_>) -> Result<()> {
    let receipts = {
        let mut statement =
            transaction.prepare("SELECT action_id, outcome_json FROM action_receipts")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>>>()?
    };

    for (action_id, outcome_json) in receipts {
        let legacy: LegacyActionOutcome =
            serde_json::from_str(&outcome_json).map_err(json_conversion_error)?;
        let outcome = legacy.into_v2()?;
        let outcome_json = serde_json::to_string(&outcome).map_err(json_conversion_error)?;
        transaction.execute(
            "UPDATE action_receipts SET outcome_json = ?1 WHERE action_id = ?2",
            params![outcome_json, action_id],
        )?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct LegacyActionOutcome {
    status: ActionStatus,
    view: LegacyTargetView,
}

impl LegacyActionOutcome {
    fn into_v2(self) -> Result<ActionOutcome> {
        let current_provider_id = self.view.current_provider_id.clone();
        let activated_provider_id = self
            .view
            .activated_snapshot
            .as_ref()
            .map(|snapshot| snapshot.provider_id);
        let providers = self
            .view
            .providers
            .into_iter()
            .enumerate()
            .map(|(position, provider)| {
                provider.into_v2(
                    position,
                    current_provider_id.as_deref(),
                    activated_provider_id,
                )
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(ActionOutcome {
            status: self.status,
            view: TargetView {
                target: self.view.target,
                management_revision: self.view.management_revision,
                view_sequence: self.view.view_sequence,
                service: self.view.service,
                mode: self.view.mode,
                takeover: self.view.takeover,
                providers,
                provider_presets: super::providers::provider_presets(),
                current_provider_id,
                serving_provider_id: self.view.serving_provider_id,
                managed_configuration: self.view.managed_configuration,
                recovery: self.view.recovery,
                activated_snapshot: self.view.activated_snapshot,
                problems: self.view.problems,
            },
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyTargetView {
    target: Target,
    management_revision: u64,
    view_sequence: u64,
    service: ServiceView,
    mode: String,
    takeover: TakeoverView,
    providers: Vec<LegacyProviderView>,
    current_provider_id: Option<String>,
    serving_provider_id: Option<String>,
    managed_configuration: ManagedConfigurationView,
    recovery: RecoveryView,
    activated_snapshot: Option<ActivatedSnapshotView>,
    problems: Vec<ControlProblem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyProviderView {
    id: String,
    name: String,
    base_url: String,
    model: String,
    credential: CredentialPresence,
}

impl LegacyProviderView {
    fn into_v2(
        self,
        position: usize,
        current_provider_id: Option<&str>,
        activated_provider_id: Option<uuid::Uuid>,
    ) -> Result<ProviderView> {
        let id = uuid::Uuid::parse_str(&self.id).map_err(json_conversion_error)?;
        let mut missing_fields = Vec::new();
        if self.base_url.is_empty() {
            missing_fields.push(ProviderRequirement::BaseUrl);
        }
        if self.model.is_empty() {
            missing_fields.push(ProviderRequirement::Model);
        }
        if self.credential == CredentialPresence::Missing {
            missing_fields.push(ProviderRequirement::Credential);
        }
        let mut active_references = Vec::new();
        if current_provider_id == Some(self.id.as_str()) {
            active_references.push(ProviderReferenceView::Current);
        }
        if activated_provider_id == Some(id) {
            active_references.push(ProviderReferenceView::ActivatedSnapshot);
        }

        Ok(ProviderView {
            id,
            position: u32::try_from(position).map_err(json_conversion_error)?,
            provider_revision: 1,
            name: self.name,
            base_url: self.base_url,
            model: self.model,
            protocol: ProviderProtocol::OpenaiResponses,
            credential: self.credential,
            completeness: if missing_fields.is_empty() {
                ProviderCompleteness::Complete
            } else {
                ProviderCompleteness::Incomplete
            },
            missing_fields,
            provenance: None,
            generated: false,
            active_references,
        })
    }
}

fn json_conversion_error(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
}
