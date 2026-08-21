use std::{collections::BTreeMap, fs, io::Read, path::Path};

use secrecy::SecretString;
use tokio_rusqlite::rusqlite::{Connection, OptionalExtension, hooks};
use toml_edit::DocumentMut;

use crate::{
    control::protocol::{
        ProviderAuthentication, ProviderImportHistoricalUsagePreview, ProviderRoutingRequirement,
        Target,
    },
    domain::provider::normalize_provider_base_url,
};

const CC_SWITCH_EXPORT_HEADER: &str = "-- CC Switch SQLite 导出";
const CC_SWITCH_SCHEMA_VERSION: u32 = 16;
const MAX_EXPORT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_PROVIDER_COUNT: usize = 256;
const MAX_ID_BYTES: usize = 256;
const MAX_NAME_BYTES: usize = 256;
const MAX_MODEL_BYTES: usize = 256;
const MAX_URL_BYTES: usize = 4096;
const MAX_CREDENTIAL_BYTES: usize = 16_384;

const PROVIDER_COLUMNS: &[&str] = &[
    "id",
    "app_type",
    "name",
    "settings_config",
    "website_url",
    "category",
    "created_at",
    "sort_index",
    "notes",
    "icon",
    "icon_color",
    "meta",
    "is_current",
    "in_failover_queue",
];

const REQUEST_LOG_COLUMNS: &[&str] = &[
    "request_id",
    "provider_id",
    "app_type",
    "model",
    "request_model",
    "pricing_model",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_creation_tokens",
    "input_token_semantics",
    "input_cost_usd",
    "output_cost_usd",
    "cache_read_cost_usd",
    "cache_creation_cost_usd",
    "total_cost_usd",
    "latency_ms",
    "first_token_ms",
    "duration_ms",
    "status_code",
    "error_message",
    "session_id",
    "provider_type",
    "is_streaming",
    "cost_multiplier",
    "created_at",
    "data_source",
];

const USAGE_ROLLUP_COLUMNS: &[&str] = &[
    "date",
    "app_type",
    "provider_id",
    "model",
    "request_model",
    "pricing_model",
    "request_count",
    "success_count",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_creation_tokens",
    "input_token_semantics",
    "total_cost_usd",
    "avg_latency_ms",
];

#[derive(Debug, thiserror::Error)]
pub(crate) enum CcSwitchMigrationError {
    #[error("CC-Switch export path is invalid")]
    InvalidPath,
    #[error("CC-Switch export is too large")]
    TooLarge,
    #[error("CC-Switch export is invalid")]
    InvalidExport,
    #[error("CC-Switch export schema is unsupported")]
    UnsupportedSchema,
}

pub(crate) struct ParsedCcSwitchMigration {
    pub(crate) providers: Vec<CcSwitchProvider>,
    pub(crate) historical_usage: CcSwitchHistoricalUsage,
}

pub(crate) struct CcSwitchProvider {
    pub(crate) source_id: String,
    pub(crate) source_position: u32,
    pub(crate) name: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) authentication: ProviderAuthentication,
    pub(crate) routing_requirement: ProviderRoutingRequirement,
    pub(crate) credential: Option<SecretString>,
}

pub(crate) struct CcSwitchHistoricalUsage {
    pub(crate) source_export_fingerprint: String,
    pub(crate) rollups: Vec<CcSwitchMigratedUsageRollup>,
    pub(crate) preview: ProviderImportHistoricalUsagePreview,
}

#[derive(Clone)]
pub(crate) struct CcSwitchMigratedUsageRollup {
    pub(crate) local_date: String,
    pub(crate) source_record_count: u64,
    pub(crate) successful_request_count: u64,
    pub(crate) failed_request_count: u64,
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) cache_creation_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) latency_observation_count: u64,
    pub(crate) total_latency_ms: u64,
}

pub(crate) fn parse_selected_export(
    path: &Path,
    target: Target,
) -> Result<ParsedCcSwitchMigration, CcSwitchMigrationError> {
    if !path.is_absolute() || path.as_os_str().len() > MAX_PATH_BYTES {
        return Err(CcSwitchMigrationError::InvalidPath);
    }
    let file = fs::File::open(path).map_err(|_| CcSwitchMigrationError::InvalidPath)?;
    let metadata = file
        .metadata()
        .map_err(|_| CcSwitchMigrationError::InvalidPath)?;
    if !metadata.is_file() {
        return Err(CcSwitchMigrationError::InvalidPath);
    }
    if metadata.len() > MAX_EXPORT_BYTES {
        return Err(CcSwitchMigrationError::TooLarge);
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| CcSwitchMigrationError::TooLarge)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_EXPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CcSwitchMigrationError::InvalidPath)?;
    if bytes.len() as u64 > MAX_EXPORT_BYTES {
        return Err(CcSwitchMigrationError::TooLarge);
    }
    let sql = std::str::from_utf8(&bytes).map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    let sql = sql.trim_start_matches('\u{feff}');
    if sql.trim_start().lines().next() != Some(CC_SWITCH_EXPORT_HEADER) {
        return Err(CcSwitchMigrationError::InvalidExport);
    }

    let connection =
        Connection::open_in_memory().map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    connection.authorizer(Some(import_authorizer));
    let imported = connection.execute_batch(sql);
    connection.authorizer(None::<fn(hooks::AuthContext<'_>) -> hooks::Authorization>);
    imported.map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    validate_schema(&connection)?;

    let providers = parse_providers(&connection, target)?;
    let source_export_fingerprint = sha256_hex(&bytes);
    let rollups = parse_historical_usage(&connection, target)?;
    let record_count = rollups.iter().try_fold(0_u64, |total, rollup| {
        total
            .checked_add(rollup.source_record_count)
            .ok_or(CcSwitchMigrationError::InvalidExport)
    })?;
    let estimated_storage_bytes = record_count
        .checked_mul(32)
        .and_then(|value| value.checked_add((rollups.len() as u64).checked_mul(256)?))
        .ok_or(CcSwitchMigrationError::InvalidExport)?;
    let preview = ProviderImportHistoricalUsagePreview {
        record_count,
        start_date: rollups.first().map(|rollup| rollup.local_date.clone()),
        end_date: rollups.last().map(|rollup| rollup.local_date.clone()),
        estimated_storage_bytes,
        selected_by_default: false,
    };
    Ok(ParsedCcSwitchMigration {
        providers,
        historical_usage: CcSwitchHistoricalUsage {
            source_export_fingerprint,
            rollups,
            preview,
        },
    })
}

fn import_authorizer(context: hooks::AuthContext<'_>) -> hooks::Authorization {
    use hooks::{AuthAction, Authorization};

    let denied = match context.action {
        AuthAction::Attach { .. }
        | AuthAction::Detach { .. }
        | AuthAction::CreateVtable { .. }
        | AuthAction::DropVtable { .. }
        | AuthAction::Unknown { .. } => true,
        AuthAction::Pragma { pragma_name, .. } => !["foreign_keys", "user_version"]
            .iter()
            .any(|allowed| pragma_name.eq_ignore_ascii_case(allowed)),
        _ => false,
    };
    if denied {
        Authorization::Deny
    } else {
        Authorization::Allow
    }
}

fn validate_schema(connection: &Connection) -> Result<(), CcSwitchMigrationError> {
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .map_err(|_| CcSwitchMigrationError::UnsupportedSchema)?;
    if version != CC_SWITCH_SCHEMA_VERSION {
        return Err(CcSwitchMigrationError::UnsupportedSchema);
    }
    for (table, expected) in [
        ("providers", PROVIDER_COLUMNS),
        ("proxy_request_logs", REQUEST_LOG_COLUMNS),
        ("usage_daily_rollups", USAGE_ROLLUP_COLUMNS),
    ] {
        let object_type = connection
            .query_row(
                "SELECT type FROM sqlite_master WHERE name = ?1",
                [table],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| CcSwitchMigrationError::UnsupportedSchema)?;
        if object_type.as_deref() != Some("table") || table_columns(connection, table)? != expected
        {
            return Err(CcSwitchMigrationError::UnsupportedSchema);
        }
    }
    let integrity = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    if integrity != "ok" {
        return Err(CcSwitchMigrationError::InvalidExport);
    }
    Ok(())
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<Vec<String>, CcSwitchMigrationError> {
    let sql = match table {
        "providers" => "PRAGMA table_info(\"providers\")",
        "proxy_request_logs" => "PRAGMA table_info(\"proxy_request_logs\")",
        "usage_daily_rollups" => "PRAGMA table_info(\"usage_daily_rollups\")",
        _ => return Err(CcSwitchMigrationError::UnsupportedSchema),
    };
    let mut statement = connection
        .prepare(sql)
        .map_err(|_| CcSwitchMigrationError::UnsupportedSchema)?;
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| CcSwitchMigrationError::UnsupportedSchema)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CcSwitchMigrationError::UnsupportedSchema)
}

fn parse_providers(
    connection: &Connection,
    target: Target,
) -> Result<Vec<CcSwitchProvider>, CcSwitchMigrationError> {
    let mut statement = connection
        .prepare(
            "SELECT id, name, settings_config
             FROM providers WHERE app_type = ?1
             ORDER BY COALESCE(sort_index, 2147483647), id",
        )
        .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    let rows = statement
        .query_map([target.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    let mut providers = Vec::new();
    let mut source_ids = std::collections::HashSet::new();
    for row in rows {
        let (source_id, name, settings_config) =
            row.map_err(|_| CcSwitchMigrationError::InvalidExport)?;
        if !source_ids.insert(source_id.clone()) {
            return Err(CcSwitchMigrationError::InvalidExport);
        }
        validate_required(&source_id, MAX_ID_BYTES)?;
        validate_required(&name, MAX_NAME_BYTES)?;
        let (base_url, model, authentication, credential) = match target {
            Target::Codex => parse_codex_settings(&settings_config)?,
            Target::Claude => parse_claude_settings(&settings_config)?,
        };
        providers.push(CcSwitchProvider {
            source_id,
            source_position: u32::try_from(providers.len())
                .map_err(|_| CcSwitchMigrationError::TooLarge)?,
            name,
            base_url,
            model,
            authentication,
            routing_requirement: ProviderRoutingRequirement::DirectCompatible,
            credential,
        });
        if providers.len() > MAX_PROVIDER_COUNT {
            return Err(CcSwitchMigrationError::TooLarge);
        }
    }
    Ok(providers)
}

fn parse_codex_settings(
    source: &str,
) -> Result<(String, String, ProviderAuthentication, Option<SecretString>), CcSwitchMigrationError>
{
    let value: serde_json::Value =
        serde_json::from_str(source).map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    let object = value
        .as_object()
        .ok_or(CcSwitchMigrationError::InvalidExport)?;
    let auth = object
        .get("auth")
        .map(|value| {
            value
                .as_object()
                .ok_or(CcSwitchMigrationError::InvalidExport)
        })
        .transpose()?;
    let credential = optional_json_string(
        auth.and_then(|auth| auth.get("OPENAI_API_KEY")),
        MAX_CREDENTIAL_BYTES,
    )?
    .map(SecretString::from);
    let config = object
        .get("config")
        .map(|value| value.as_str().ok_or(CcSwitchMigrationError::InvalidExport))
        .transpose()?
        .unwrap_or("");
    let document = if config.trim().is_empty() {
        DocumentMut::new()
    } else {
        config
            .parse::<DocumentMut>()
            .map_err(|_| CcSwitchMigrationError::InvalidExport)?
    };
    let model = optional_bounded(
        document.get("model").and_then(|item| item.as_str()),
        MAX_MODEL_BYTES,
    )?;
    let provider_key = document
        .get("model_provider")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let base_url = document
        .get("model_providers")
        .and_then(|item| item.as_table_like())
        .and_then(|providers| providers.get(provider_key))
        .and_then(|item| item.as_table_like())
        .and_then(|provider| provider.get("base_url"))
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let base_url = normalized_optional_url(base_url)?;
    Ok((
        base_url,
        model,
        ProviderAuthentication::OpenaiBearer,
        credential,
    ))
}

fn parse_claude_settings(
    source: &str,
) -> Result<(String, String, ProviderAuthentication, Option<SecretString>), CcSwitchMigrationError>
{
    let value: serde_json::Value =
        serde_json::from_str(source).map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    let object = value
        .as_object()
        .ok_or(CcSwitchMigrationError::InvalidExport)?;
    let env = object
        .get("env")
        .map(|value| {
            value
                .as_object()
                .ok_or(CcSwitchMigrationError::InvalidExport)
        })
        .transpose()?;
    let base_url = optional_json_string(
        env.and_then(|env| env.get("ANTHROPIC_BASE_URL")),
        MAX_URL_BYTES,
    )?
    .unwrap_or_default();
    let model = optional_json_string(
        env.and_then(|env| env.get("ANTHROPIC_MODEL")),
        MAX_MODEL_BYTES,
    )?
    .or_else(|| {
        env.and_then(|env| env.get("ANTHROPIC_DEFAULT_SONNET_MODEL"))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    })
    .unwrap_or_default();
    if model.len() > MAX_MODEL_BYTES {
        return Err(CcSwitchMigrationError::InvalidExport);
    }
    let auth_token = optional_json_string(
        env.and_then(|env| env.get("ANTHROPIC_AUTH_TOKEN")),
        MAX_CREDENTIAL_BYTES,
    )?;
    let api_key = optional_json_string(
        env.and_then(|env| env.get("ANTHROPIC_API_KEY")),
        MAX_CREDENTIAL_BYTES,
    )?;
    let (authentication, credential) = match (auth_token, api_key) {
        (Some(_), Some(_)) => return Err(CcSwitchMigrationError::InvalidExport),
        (Some(value), None) => (
            ProviderAuthentication::AnthropicBearer,
            Some(SecretString::from(value)),
        ),
        (None, Some(value)) => (
            ProviderAuthentication::AnthropicApiKey,
            Some(SecretString::from(value)),
        ),
        (None, None) => (ProviderAuthentication::AnthropicBearer, None),
    };
    Ok((
        normalized_optional_url(&base_url)?,
        model,
        authentication,
        credential,
    ))
}

fn parse_historical_usage(
    connection: &Connection,
    target: Target,
) -> Result<Vec<CcSwitchMigratedUsageRollup>, CcSwitchMigrationError> {
    let mut rollups = BTreeMap::<String, CcSwitchMigratedUsageRollup>::new();
    {
        let mut statement = connection
            .prepare(
                "SELECT date(created_at, 'unixepoch', 'localtime') AS local_date,
                        COUNT(*),
                        SUM(CASE WHEN status_code BETWEEN 200 AND 399 THEN 1 ELSE 0 END),
                        SUM(input_tokens), SUM(cache_read_tokens),
                        SUM(cache_creation_tokens), SUM(output_tokens), SUM(latency_ms)
                 FROM proxy_request_logs
                 WHERE app_type = ?1
                 GROUP BY local_date ORDER BY local_date",
            )
            .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
        let rows = statement
            .query_map([target.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, u64>(6)?,
                    row.get::<_, u64>(7)?,
                ))
            })
            .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
        for row in rows {
            let (date, count, success, input, cached, creation, output, latency) =
                row.map_err(|_| CcSwitchMigrationError::InvalidExport)?;
            let failed = count
                .checked_sub(success)
                .ok_or(CcSwitchMigrationError::InvalidExport)?;
            merge_rollup(
                &mut rollups,
                CcSwitchMigratedUsageRollup {
                    local_date: date,
                    source_record_count: count,
                    successful_request_count: success,
                    failed_request_count: failed,
                    input_tokens: input,
                    cached_input_tokens: cached,
                    cache_creation_input_tokens: creation,
                    output_tokens: output,
                    latency_observation_count: count,
                    total_latency_ms: latency,
                },
            )?;
        }
    }
    {
        let mut statement = connection
            .prepare(
                "SELECT date, request_count, success_count, input_tokens,
                        cache_read_tokens, cache_creation_tokens, output_tokens,
                        avg_latency_ms
                 FROM usage_daily_rollups
                 WHERE app_type = ?1 ORDER BY date",
            )
            .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
        let rows = statement
            .query_map([target.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, u64>(6)?,
                    row.get::<_, u64>(7)?,
                ))
            })
            .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
        for row in rows {
            let (date, count, success, input, cached, creation, output, average_latency) =
                row.map_err(|_| CcSwitchMigrationError::InvalidExport)?;
            let failed = count
                .checked_sub(success)
                .ok_or(CcSwitchMigrationError::InvalidExport)?;
            let total_latency_ms = average_latency
                .checked_mul(count)
                .ok_or(CcSwitchMigrationError::InvalidExport)?;
            merge_rollup(
                &mut rollups,
                CcSwitchMigratedUsageRollup {
                    local_date: date,
                    source_record_count: count,
                    successful_request_count: success,
                    failed_request_count: failed,
                    input_tokens: input,
                    cached_input_tokens: cached,
                    cache_creation_input_tokens: creation,
                    output_tokens: output,
                    latency_observation_count: count,
                    total_latency_ms,
                },
            )?;
        }
    }
    Ok(rollups.into_values().collect())
}

fn merge_rollup(
    rollups: &mut BTreeMap<String, CcSwitchMigratedUsageRollup>,
    incoming: CcSwitchMigratedUsageRollup,
) -> Result<(), CcSwitchMigrationError> {
    validate_local_date(&incoming.local_date)?;
    if incoming.source_record_count == 0 {
        return Ok(());
    }
    let Some(current) = rollups.get_mut(&incoming.local_date) else {
        rollups.insert(incoming.local_date.clone(), incoming);
        return Ok(());
    };
    macro_rules! add {
        ($field:ident) => {
            current.$field = current
                .$field
                .checked_add(incoming.$field)
                .ok_or(CcSwitchMigrationError::InvalidExport)?;
        };
    }
    add!(source_record_count);
    add!(successful_request_count);
    add!(failed_request_count);
    add!(input_tokens);
    add!(cached_input_tokens);
    add!(cache_creation_input_tokens);
    add!(output_tokens);
    add!(latency_observation_count);
    add!(total_latency_ms);
    Ok(())
}

fn validate_local_date(value: &str) -> Result<(), CcSwitchMigrationError> {
    let shape_valid = value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
    if !shape_valid {
        return Err(CcSwitchMigrationError::InvalidExport);
    }
    let year = value[0..4]
        .parse::<u32>()
        .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    let month = value[5..7]
        .parse::<u32>()
        .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    let day = value[8..10]
        .parse::<u32>()
        .map_err(|_| CcSwitchMigrationError::InvalidExport)?;
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => 0,
    };
    if year > 0 && day > 0 && day <= days_in_month {
        Ok(())
    } else {
        Err(CcSwitchMigrationError::InvalidExport)
    }
}

fn optional_json_string(
    value: Option<&serde_json::Value>,
    max_bytes: usize,
) -> Result<Option<String>, CcSwitchMigrationError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or(CcSwitchMigrationError::InvalidExport)?
        .trim();
    if value.len() > max_bytes {
        return Err(CcSwitchMigrationError::TooLarge);
    }
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn optional_bounded(
    value: Option<&str>,
    max_bytes: usize,
) -> Result<String, CcSwitchMigrationError> {
    let value = value.unwrap_or("").trim();
    if value.len() > max_bytes {
        return Err(CcSwitchMigrationError::TooLarge);
    }
    Ok(value.to_owned())
}

fn normalized_optional_url(value: &str) -> Result<String, CcSwitchMigrationError> {
    let value = value.trim();
    if value.len() > MAX_URL_BYTES {
        return Err(CcSwitchMigrationError::TooLarge);
    }
    if value.is_empty() {
        return Ok(String::new());
    }
    normalize_provider_base_url(value).map_err(|_| CcSwitchMigrationError::InvalidExport)
}

fn validate_required(value: &str, max_bytes: usize) -> Result<(), CcSwitchMigrationError> {
    if value.trim().is_empty() || value.len() > max_bytes {
        Err(CcSwitchMigrationError::InvalidExport)
    } else {
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use secrecy::ExposeSecret;
    use tempfile::TempDir;

    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/cc-switch-v3.19.2-export.sql");
    const MANIFEST: &str =
        include_str!("../../tests/fixtures/cc-switch-v3.19.2-export.manifest.json");

    fn selected_file(contents: &str) -> (TempDir, PathBuf) {
        let directory = TempDir::new().expect("temporary export directory");
        let path = directory.path().join("selected-export.sql");
        fs::write(&path, contents).expect("write selected export");
        (directory, path)
    }

    #[test]
    fn pinned_export_projects_codex_providers_and_default_off_usage() {
        let (_directory, path) = selected_file(FIXTURE);
        let parsed = parse_selected_export(&path, Target::Codex).expect("valid pinned export");

        assert_eq!(parsed.providers.len(), 1);
        let provider = &parsed.providers[0];
        assert_eq!(provider.source_id, "cc-codex-1");
        assert_eq!(provider.name, "Same Name");
        assert_eq!(provider.base_url, "https://codex-export.example/v1");
        assert_eq!(provider.model, "gpt-5.6-sol");
        assert_eq!(
            provider.authentication,
            ProviderAuthentication::OpenaiBearer
        );
        assert_eq!(
            provider
                .credential
                .as_ref()
                .expect("credential")
                .expose_secret(),
            "ccswitch-codex-credential-fixture"
        );
        assert_eq!(parsed.historical_usage.preview.record_count, 5);
        assert_eq!(
            parsed.historical_usage.preview.start_date.as_deref(),
            Some("2025-12-31")
        );
        assert_eq!(
            parsed.historical_usage.preview.end_date.as_deref(),
            Some("2026-01-01")
        );
        assert!(!parsed.historical_usage.preview.selected_by_default);
        assert_eq!(parsed.historical_usage.rollups.len(), 2);
    }

    #[test]
    fn pinned_export_filters_to_the_selected_target() {
        let (_directory, path) = selected_file(FIXTURE);
        let parsed = parse_selected_export(&path, Target::Claude).expect("valid pinned export");

        assert_eq!(parsed.providers.len(), 1);
        let provider = &parsed.providers[0];
        assert_eq!(provider.source_id, "cc-claude-1");
        assert_eq!(provider.base_url, "https://claude-export.example/");
        assert_eq!(provider.model, "claude-sonnet-4-6");
        assert_eq!(
            provider.authentication,
            ProviderAuthentication::AnthropicBearer
        );
        assert_eq!(parsed.historical_usage.preview.record_count, 1);
    }

    #[test]
    fn parser_rejects_attach_before_reading_any_projected_rows() {
        let hostile = FIXTURE.replacen(
            "BEGIN TRANSACTION;",
            "ATTACH DATABASE '/tmp/muxvia-cc-switch-escape.db' AS escaped;\nBEGIN TRANSACTION;",
            1,
        );
        let (_directory, path) = selected_file(&hostile);
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::InvalidExport)
        ));
        assert!(!Path::new("/tmp/muxvia-cc-switch-escape.db").exists());
    }

    #[test]
    fn parser_rejects_unsupported_or_corrupt_schema_without_partial_interpretation() {
        let unsupported = FIXTURE.replacen("PRAGMA user_version=16;", "PRAGMA user_version=15;", 1);
        let (_directory, path) = selected_file(&unsupported);
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::UnsupportedSchema)
        ));

        let invalid_date = FIXTURE.replacen("'2025-12-31'", "'2025-13-31'", 1);
        let (_directory, path) = selected_file(&invalid_date);
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::InvalidExport)
        ));

        let missing_column = FIXTURE.replacen("data_source TEXT", "data_origin TEXT", 1);
        let (_directory, path) = selected_file(&missing_column);
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::UnsupportedSchema)
        ));
    }

    #[test]
    fn parser_rejects_malformed_provider_shapes_and_duplicate_source_identities() {
        let invalid_json = FIXTURE.replacen("{\"auth\"", "{broken", 1);
        let (_directory, path) = selected_file(&invalid_json);
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::InvalidExport)
        ));

        let invalid_toml =
            FIXTURE.replacen("model_provider = \\\"custom\\\"", "model_provider = [", 1);
        let (_directory, path) = selected_file(&invalid_toml);
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::InvalidExport)
        ));

        let duplicate = FIXTURE
            .replacen("PRIMARY KEY (id, app_type)", "CHECK (length(id) > 0)", 1)
            .replacen(
                "INSERT INTO proxy_request_logs VALUES (",
                "INSERT INTO providers SELECT * FROM providers WHERE id = 'cc-codex-1';\nINSERT INTO proxy_request_logs VALUES (",
                1,
            );
        let (_directory, path) = selected_file(&duplicate);
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::InvalidExport)
        ));
    }

    #[test]
    fn parser_rejects_corrupt_sql_virtual_tables_and_oversized_selected_files() {
        let corrupt = format!("{FIXTURE}\nNOT VALID SQLITE;");
        let (_directory, path) = selected_file(&corrupt);
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::InvalidExport)
        ));

        let virtual_table = FIXTURE.replacen(
            "BEGIN TRANSACTION;",
            "CREATE VIRTUAL TABLE denied USING fts5(value);\nBEGIN TRANSACTION;",
            1,
        );
        let (_directory, path) = selected_file(&virtual_table);
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::InvalidExport)
        ));

        let (directory, path) = selected_file(CC_SWITCH_EXPORT_HEADER);
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_EXPORT_BYTES + 1).unwrap();
        assert!(matches!(
            parse_selected_export(&path, Target::Codex),
            Err(CcSwitchMigrationError::TooLarge)
        ));
        drop(directory);
    }

    #[test]
    fn parser_requires_one_absolute_operator_selected_regular_file() {
        assert!(matches!(
            parse_selected_export(Path::new("relative-export.sql"), Target::Codex),
            Err(CcSwitchMigrationError::InvalidPath)
        ));
        let oversized_path = PathBuf::from(format!("/{}", "x".repeat(MAX_PATH_BYTES)));
        assert!(matches!(
            parse_selected_export(&oversized_path, Target::Codex),
            Err(CcSwitchMigrationError::InvalidPath)
        ));
        let directory = TempDir::new().expect("temporary directory");
        assert!(matches!(
            parse_selected_export(directory.path(), Target::Codex),
            Err(CcSwitchMigrationError::InvalidPath)
        ));
    }

    #[test]
    fn compatibility_manifest_binds_the_fixture_hash_and_source_commit() {
        let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("manifest JSON");
        assert_eq!(
            manifest["commit"],
            "43eaf07355af145aebfee301801779e824d4c221"
        );
        assert_eq!(manifest["fixtureSha256"], sha256_hex(FIXTURE.as_bytes()));
    }
}
