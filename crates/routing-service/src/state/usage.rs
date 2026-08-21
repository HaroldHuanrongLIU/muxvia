use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio_rusqlite::rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

use crate::{
    control::protocol::{
        DailyUsageRollup, NativeUsageRecordSummary, PricingCatalogUpdateOutcome, RequestUsageView,
        Target, UsageActivityEntry, UsageActivityPage, UsageClearOutcome, UsageRetentionOutcome,
    },
    native_usage::{NativeUsageError, ParsedNativeFile},
    request_history::{PricingCatalog, PricingSnapshot, RequestUsage},
};

use super::{StateStore, request_records::StoredRequestRecord};

const CURSOR_VERSION: &str = "usage-activity-v1";

impl StateStore {
    pub(crate) async fn import_native_usage(
        &self,
        target: Target,
        files: Vec<ParsedNativeFile>,
        now_unix_ms: u64,
    ) -> Result<u64, NativeUsageError> {
        self.connection
            .call(move |connection| -> Result<u64, NativeUsageError> {
                let transaction = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
                let catalog = active_catalog(&transaction)?;
                let mut imported = 0_u64;
                for file in files {
                    let prior = transaction
                        .query_row(
                            "SELECT modified_unix_nanos, byte_length, completed_line_count
                             FROM native_usage_import_cursors
                             WHERE target = ?1 AND source_fingerprint = ?2",
                            params![target.as_str(), file.source_fingerprint],
                            |row| {
                                Ok((
                                    row.get::<_, u64>(0)?,
                                    row.get::<_, u64>(1)?,
                                    row.get::<_, u64>(2)?,
                                ))
                            },
                        )
                        .optional()
                        .map_err(sql_error)?;
                    if prior
                        == Some((
                            file.modified_unix_nanos,
                            file.byte_length,
                            file.completed_line_count,
                        ))
                    {
                        continue;
                    }
                    let first_new_line = prior
                        .filter(|(_, byte_length, lines)| {
                            file.byte_length >= *byte_length && file.completed_line_count >= *lines
                        })
                        .map(|(_, _, lines)| lines)
                        .unwrap_or(0);
                    for record in file
                        .records
                        .iter()
                        .filter(|record| record.line_number > first_new_line)
                    {
                        let id = Uuid::new_v4();
                        let inserted = transaction
                            .execute(
                                "INSERT OR IGNORE INTO native_usage_records
                                   (id, target, source_record_fingerprint, model,
                                    observed_at_unix_ms, input_tokens, cached_input_tokens,
                                    cache_creation_input_tokens, output_tokens)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                                params![
                                    id.to_string(),
                                    target.as_str(),
                                    record.source_record_fingerprint,
                                    record.model,
                                    record.observed_at_unix_ms,
                                    record.usage.input_tokens,
                                    record.usage.cached_input_tokens,
                                    record.usage.cache_creation_input_tokens,
                                    record.usage.output_tokens,
                                ],
                            )
                            .map_err(sql_error)?;
                        if inserted == 0 {
                            continue;
                        }
                        if let Some(snapshot) = catalog
                            .price(&record.model, record.usage, record.observed_at_unix_ms)
                            .map_err(|_| NativeUsageError::Unavailable)?
                        {
                            insert_native_pricing_snapshot(&transaction, id, &snapshot)?;
                        }
                        imported = imported
                            .checked_add(1)
                            .ok_or(NativeUsageError::Unavailable)?;
                    }
                    transaction
                        .execute(
                            "INSERT INTO native_usage_import_cursors
                               (target, source_fingerprint, modified_unix_nanos, byte_length,
                                completed_line_count)
                             VALUES (?1, ?2, ?3, ?4, ?5)
                             ON CONFLICT(target, source_fingerprint) DO UPDATE SET
                               modified_unix_nanos = excluded.modified_unix_nanos,
                               byte_length = excluded.byte_length,
                               completed_line_count = excluded.completed_line_count",
                            params![
                                target.as_str(),
                                file.source_fingerprint,
                                file.modified_unix_nanos,
                                file.byte_length,
                                file.completed_line_count,
                            ],
                        )
                        .map_err(sql_error)?;
                }
                let detailed_retention_days = transaction
                    .query_row(
                        "SELECT detailed_retention_days FROM usage_settings WHERE singleton = 1",
                        [],
                        |row| row.get::<_, u16>(0),
                    )
                    .map_err(sql_error)?;
                roll_up_usage(&transaction, target, detailed_retention_days, now_unix_ms)?;
                transaction.commit().map_err(sql_error)?;
                Ok(imported)
            })
            .await
            .map_err(|_| NativeUsageError::Unavailable)
    }

    pub(crate) async fn list_usage_activity(
        &self,
        target: Target,
        before_cursor: Option<&str>,
        limit: u16,
    ) -> Result<UsageActivityPage, NativeUsageError> {
        if !(1..=100).contains(&limit) {
            return Err(NativeUsageError::InvalidRequest);
        }
        let before = before_cursor
            .map(|cursor| decode_cursor(target, cursor))
            .transpose()?;
        let target_name = target.as_str().to_owned();
        let (mut items, detailed_retention_days, catalog_version) = self
            .connection
            .call(move |connection| -> Result<_, NativeUsageError> {
                let detailed_retention_days = connection
                    .query_row(
                        "SELECT detailed_retention_days FROM usage_settings WHERE singleton = 1",
                        [],
                        |row| row.get::<_, u16>(0),
                    )
                    .map_err(sql_error)?;
                let catalog_version = connection
                    .query_row(
                        "SELECT catalog_version FROM pricing_catalog_state WHERE singleton = 1",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(sql_error)?;
                let mut items = Vec::new();
                {
                    let mut statement = connection
                        .prepare(
                            "SELECT request.sequence, request.id, request.plan_id,
                                    request.plan_epoch, request.provider_id,
                                    request.provider_name, request.model, request.protocol,
                                    request.started_at_unix_ms, request.finished_at_unix_ms,
                                    request.latency_ms, request.outcome, request.http_status,
                                    request.usage_observed, request.input_tokens,
                                    request.cached_input_tokens,
                                    request.cache_creation_input_tokens, request.output_tokens,
                                    request.error_payload IS NOT NULL,
                                    request.error_payload_truncated,
                                    pricing.estimated_cost_nano_usd
                             FROM request_records request
                             LEFT JOIN pricing_snapshots pricing
                               ON pricing.request_record_id = request.id
                             WHERE request.target = ?1",
                        )
                        .map_err(sql_error)?;
                    let rows = statement
                        .query_map([&target_name], StoredRequestRecord::from_row)
                        .map_err(sql_error)?;
                    for row in rows {
                        let row = row.map_err(sql_error)?;
                        let sequence = row.sequence;
                        let summary = row
                            .into_summary()
                            .map_err(|_| NativeUsageError::Unavailable)?;
                        items.push(ActivityItem {
                            key: ActivityKey(summary.finished_at_unix_ms, 2, sequence),
                            entry: UsageActivityEntry::RequestRecord { record: summary },
                        });
                    }
                }
                {
                    let mut statement = connection
                        .prepare(
                            "SELECT native.sequence, native.id, native.model,
                                    native.observed_at_unix_ms, native.input_tokens,
                                    native.cached_input_tokens,
                                    native.cache_creation_input_tokens, native.output_tokens,
                                    pricing.estimated_cost_nano_usd
                             FROM native_usage_records native
                             LEFT JOIN native_usage_pricing_snapshots pricing
                               ON pricing.native_usage_record_id = native.id
                             WHERE native.target = ?1",
                        )
                        .map_err(sql_error)?;
                    let rows = statement
                        .query_map([&target_name], |row| {
                            Ok((
                                row.get::<_, u64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, u64>(3)?,
                                row.get::<_, u64>(4)?,
                                row.get::<_, u64>(5)?,
                                row.get::<_, u64>(6)?,
                                row.get::<_, u64>(7)?,
                                row.get::<_, Option<u64>>(8)?,
                            ))
                        })
                        .map_err(sql_error)?;
                    for row in rows {
                        let (sequence, id, model, observed_at_unix_ms, input, cached, creation, output, cost) =
                            row.map_err(sql_error)?;
                        let id = Uuid::parse_str(&id).map_err(|_| NativeUsageError::Unavailable)?;
                        items.push(ActivityItem {
                            key: ActivityKey(observed_at_unix_ms, 1, sequence),
                            entry: UsageActivityEntry::NativeUsageRecord {
                                record: NativeUsageRecordSummary {
                                    id,
                                    model,
                                    observed_at_unix_ms,
                                    usage: usage_view(input, cached, creation, output),
                                    estimated_cost_nano_usd: cost,
                                },
                            },
                        });
                    }
                }
                {
                    let mut statement = connection
                        .prepare(
                            "SELECT local_date, request_record_count,
                                    native_usage_record_count, successful_request_count,
                                    failed_request_count, input_tokens, cached_input_tokens,
                                    cache_creation_input_tokens, output_tokens,
                                    priced_record_count, unpriced_record_count,
                                    estimated_cost_nano_usd, latency_observation_count,
                                    total_latency_ms,
                                    CAST(strftime('%s', local_date || ' 23:59:59', 'utc') AS INTEGER)
                                      * 1000
                             FROM daily_usage_rollups WHERE target = ?1",
                        )
                        .map_err(sql_error)?;
                    let rows = statement
                        .query_map([&target_name], |row| {
                            Ok((
                                DailyUsageRollup {
                                    local_date: row.get(0)?,
                                    request_record_count: row.get(1)?,
                                    native_usage_record_count: row.get(2)?,
                                    successful_request_count: row.get(3)?,
                                    failed_request_count: row.get(4)?,
                                    usage: usage_view(row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?),
                                    priced_record_count: row.get(9)?,
                                    unpriced_record_count: row.get(10)?,
                                    estimated_cost_nano_usd: row.get(11)?,
                                    latency_observation_count: row.get(12)?,
                                    total_latency_ms: row.get(13)?,
                                },
                                row.get::<_, u64>(14)?,
                            ))
                        })
                        .map_err(sql_error)?;
                    for row in rows {
                        let (rollup, sort_time) = row.map_err(sql_error)?;
                        items.push(ActivityItem {
                            key: ActivityKey(sort_time, 0, 0),
                            entry: UsageActivityEntry::DailyUsageRollup { rollup },
                        });
                    }
                }
                Ok((items, detailed_retention_days, catalog_version))
            })
            .await
            .map_err(|_| NativeUsageError::Unavailable)?;
        items.sort_by_key(|item| std::cmp::Reverse(item.key));
        if let Some(before) = before {
            items.retain(|item| item.key < before);
        }
        let has_more = items.len() > usize::from(limit);
        items.truncate(usize::from(limit));
        let next_cursor = has_more
            .then(|| items.last().map(|item| encode_cursor(target, item.key)))
            .flatten();
        Ok(UsageActivityPage {
            target,
            entries: items.into_iter().map(|item| item.entry).collect(),
            next_cursor,
            detailed_retention_days,
            catalog_version,
        })
    }

    pub(crate) async fn apply_usage_retention(
        &self,
        target: Target,
        detailed_retention_days: u16,
        now_unix_ms: u64,
    ) -> Result<UsageRetentionOutcome, NativeUsageError> {
        self.connection
            .call(move |connection| -> Result<_, NativeUsageError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)
                        .map_err(sql_error)?;
                transaction
                    .execute(
                        "UPDATE usage_settings SET detailed_retention_days = ?1 WHERE singleton = 1",
                        [detailed_retention_days],
                    )
                    .map_err(sql_error)?;
                let outcome = roll_up_usage(
                    &transaction,
                    target,
                    detailed_retention_days,
                    now_unix_ms,
                )?;
                transaction.commit().map_err(sql_error)?;
                Ok(outcome)
            })
            .await
            .map_err(|_| NativeUsageError::Unavailable)
    }

    pub(crate) async fn clear_usage(
        &self,
        target: Target,
    ) -> Result<UsageClearOutcome, NativeUsageError> {
        self.connection
            .call(move |connection| -> Result<_, NativeUsageError> {
                let transaction = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
                let request_records = count(&transaction, "request_records")?;
                let native_records = count(&transaction, "native_usage_records")?;
                let rollups = count(&transaction, "daily_usage_rollups")?;
                let cursors = count(&transaction, "native_usage_import_cursors")?;
                transaction
                    .execute("DELETE FROM request_records", [])
                    .map_err(sql_error)?;
                transaction
                    .execute("DELETE FROM native_usage_records", [])
                    .map_err(sql_error)?;
                transaction
                    .execute("DELETE FROM daily_usage_rollups", [])
                    .map_err(sql_error)?;
                transaction
                    .execute("DELETE FROM native_usage_import_cursors", [])
                    .map_err(sql_error)?;
                transaction.commit().map_err(sql_error)?;
                Ok(UsageClearOutcome {
                    target,
                    cleared_request_records: request_records,
                    cleared_native_usage_records: native_records,
                    cleared_daily_rollups: rollups,
                    cleared_import_cursors: cursors,
                })
            })
            .await
            .map_err(|_| NativeUsageError::Unavailable)
    }

    pub(crate) async fn replace_pricing_catalog(
        &self,
        target: Target,
        catalog: PricingCatalog,
        priced_at_unix_ms: u64,
    ) -> Result<PricingCatalogUpdateOutcome, NativeUsageError> {
        let version = catalog.version().to_owned();
        let source = catalog.source().to_owned();
        let json = catalog
            .to_json()
            .map_err(|_| NativeUsageError::InvalidCatalog)?;
        self.connection
            .call(move |connection| -> Result<_, NativeUsageError> {
                let transaction = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
                let request_rows = read_unpriced_requests(&transaction)?;
                let native_rows = read_unpriced_native(&transaction)?;
                transaction
                    .execute(
                        "UPDATE pricing_catalog_state
                         SET catalog_version = ?1, source = ?2, catalog_json = ?3,
                             updated_at_unix_ms = ?4
                         WHERE singleton = 1",
                        params![version, source, json, priced_at_unix_ms],
                    )
                    .map_err(sql_error)?;
                let mut request_count = 0_u64;
                for (id, model, usage) in request_rows {
                    let Some(snapshot) = catalog
                        .price(&model, usage, priced_at_unix_ms)
                        .map_err(|_| NativeUsageError::InvalidCatalog)?
                    else {
                        continue;
                    };
                    insert_request_snapshot(&transaction, &id, &snapshot)?;
                    request_count = checked_add(request_count, 1)?;
                }
                let mut native_count = 0_u64;
                for (id, model, usage) in native_rows {
                    let Some(snapshot) = catalog
                        .price(&model, usage, priced_at_unix_ms)
                        .map_err(|_| NativeUsageError::InvalidCatalog)?
                    else {
                        continue;
                    };
                    insert_native_pricing_snapshot(&transaction, id, &snapshot)?;
                    native_count = checked_add(native_count, 1)?;
                }
                transaction.commit().map_err(sql_error)?;
                Ok(PricingCatalogUpdateOutcome {
                    target,
                    catalog_version: version,
                    source,
                    backfilled_request_records: request_count,
                    backfilled_native_usage_records: native_count,
                })
            })
            .await
            .map_err(|_| NativeUsageError::Unavailable)
    }
}

fn roll_up_usage(
    transaction: &Transaction<'_>,
    target: Target,
    detailed_retention_days: u16,
    now_unix_ms: u64,
) -> Result<UsageRetentionOutcome, NativeUsageError> {
    let cutoff = transaction
        .query_row(
            "SELECT date(?1 / 1000, 'unixepoch', 'localtime', printf('-%d days', ?2 - 1))",
            params![now_unix_ms, detailed_retention_days],
            |row| row.get::<_, String>(0),
        )
        .map_err(sql_error)?;
    let dates = {
        let mut statement = transaction
            .prepare(
                "SELECT local_date FROM (
                   SELECT DISTINCT date(finished_at_unix_ms / 1000, 'unixepoch', 'localtime') AS local_date
                   FROM request_records WHERE target = ?1
                   UNION
                   SELECT DISTINCT date(observed_at_unix_ms / 1000, 'unixepoch', 'localtime') AS local_date
                   FROM native_usage_records WHERE target = ?1
                 ) WHERE local_date < ?2 ORDER BY local_date",
            )
            .map_err(sql_error)?;
        statement
            .query_map(params![target.as_str(), cutoff], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?
    };
    let mut pruned_requests = 0_u64;
    let mut pruned_native = 0_u64;
    for date in &dates {
        let mut aggregate = read_rollup(transaction, target, date)?.unwrap_or_default();
        let (requests, native) = aggregate_date(transaction, target, date, &mut aggregate)?;
        write_rollup(transaction, target, date, &aggregate)?;
        transaction
            .execute(
                "DELETE FROM request_records
                 WHERE target = ?1
                   AND date(finished_at_unix_ms / 1000, 'unixepoch', 'localtime') = ?2",
                params![target.as_str(), date],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "DELETE FROM native_usage_records
                 WHERE target = ?1
                   AND date(observed_at_unix_ms / 1000, 'unixepoch', 'localtime') = ?2",
                params![target.as_str(), date],
            )
            .map_err(sql_error)?;
        pruned_requests = checked_add(pruned_requests, requests)?;
        pruned_native = checked_add(pruned_native, native)?;
    }
    Ok(UsageRetentionOutcome {
        target,
        detailed_retention_days,
        rolled_up_days: dates.len() as u64,
        pruned_request_records: pruned_requests,
        pruned_native_usage_records: pruned_native,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ActivityKey(u64, u8, u64);

struct ActivityItem {
    key: ActivityKey,
    entry: UsageActivityEntry,
}

#[derive(Default)]
struct RollupAccumulator {
    request_record_count: u64,
    native_usage_record_count: u64,
    successful_request_count: u64,
    failed_request_count: u64,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    output_tokens: u64,
    priced_record_count: u64,
    unpriced_record_count: u64,
    estimated_cost_nano_usd: u64,
    latency_observation_count: u64,
    total_latency_ms: u64,
}

fn active_catalog(transaction: &Transaction<'_>) -> Result<PricingCatalog, NativeUsageError> {
    let json = transaction
        .query_row(
            "SELECT catalog_json FROM pricing_catalog_state WHERE singleton = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(sql_error)?;
    PricingCatalog::from_json(&json).map_err(|_| NativeUsageError::Unavailable)
}

fn insert_native_pricing_snapshot(
    transaction: &Transaction<'_>,
    id: Uuid,
    snapshot: &PricingSnapshot,
) -> Result<(), NativeUsageError> {
    transaction
        .execute(
            "INSERT INTO native_usage_pricing_snapshots
               (native_usage_record_id, catalog_version, source, source_model,
                input_nano_usd_per_million, output_nano_usd_per_million,
                cache_read_multiplier_ppm, cache_creation_multiplier_ppm,
                priced_at_unix_ms, estimated_cost_nano_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id.to_string(),
                snapshot.catalog_version,
                snapshot.source,
                snapshot.source_model,
                snapshot.input_nano_usd_per_million,
                snapshot.output_nano_usd_per_million,
                snapshot.cache_read_multiplier_ppm,
                snapshot.cache_creation_multiplier_ppm,
                snapshot.priced_at_unix_ms,
                snapshot.estimated_cost_nano_usd,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn insert_request_snapshot(
    transaction: &Transaction<'_>,
    id: &str,
    snapshot: &PricingSnapshot,
) -> Result<(), NativeUsageError> {
    transaction
        .execute(
            "INSERT INTO pricing_snapshots
               (request_record_id, catalog_version, source, source_model,
                input_nano_usd_per_million, output_nano_usd_per_million,
                cache_read_multiplier_ppm, cache_creation_multiplier_ppm,
                priced_at_unix_ms, estimated_cost_nano_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                snapshot.catalog_version,
                snapshot.source,
                snapshot.source_model,
                snapshot.input_nano_usd_per_million,
                snapshot.output_nano_usd_per_million,
                snapshot.cache_read_multiplier_ppm,
                snapshot.cache_creation_multiplier_ppm,
                snapshot.priced_at_unix_ms,
                snapshot.estimated_cost_nano_usd,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn read_unpriced_requests(
    transaction: &Transaction<'_>,
) -> Result<Vec<(String, String, RequestUsage)>, NativeUsageError> {
    let mut statement = transaction
        .prepare(
            "SELECT request.id, request.model, request.input_tokens,
                    request.cached_input_tokens, request.cache_creation_input_tokens,
                    request.output_tokens
             FROM request_records request
             LEFT JOIN pricing_snapshots pricing ON pricing.request_record_id = request.id
             WHERE request.usage_observed = 1 AND pricing.request_record_id IS NULL",
        )
        .map_err(sql_error)?;
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                RequestUsage {
                    input_tokens: row.get(2)?,
                    cached_input_tokens: row.get(3)?,
                    cache_creation_input_tokens: row.get(4)?,
                    output_tokens: row.get(5)?,
                },
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn read_unpriced_native(
    transaction: &Transaction<'_>,
) -> Result<Vec<(Uuid, String, RequestUsage)>, NativeUsageError> {
    let mut statement = transaction
        .prepare(
            "SELECT native.id, native.model, native.input_tokens,
                    native.cached_input_tokens, native.cache_creation_input_tokens,
                    native.output_tokens
             FROM native_usage_records native
             LEFT JOIN native_usage_pricing_snapshots pricing
               ON pricing.native_usage_record_id = native.id
             WHERE pricing.native_usage_record_id IS NULL",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                RequestUsage {
                    input_tokens: row.get(2)?,
                    cached_input_tokens: row.get(3)?,
                    cache_creation_input_tokens: row.get(4)?,
                    output_tokens: row.get(5)?,
                },
            ))
        })
        .map_err(sql_error)?;
    rows.map(|row| {
        let (id, model, usage) = row.map_err(sql_error)?;
        Ok((
            Uuid::parse_str(&id).map_err(|_| NativeUsageError::Unavailable)?,
            model,
            usage,
        ))
    })
    .collect()
}

fn read_rollup(
    transaction: &Transaction<'_>,
    target: Target,
    date: &str,
) -> Result<Option<RollupAccumulator>, NativeUsageError> {
    transaction
        .query_row(
            "SELECT request_record_count, native_usage_record_count,
                    successful_request_count, failed_request_count, input_tokens,
                    cached_input_tokens, cache_creation_input_tokens, output_tokens,
                    priced_record_count, unpriced_record_count, estimated_cost_nano_usd,
                    latency_observation_count, total_latency_ms
             FROM daily_usage_rollups WHERE target = ?1 AND local_date = ?2",
            params![target.as_str(), date],
            |row| {
                Ok(RollupAccumulator {
                    request_record_count: row.get(0)?,
                    native_usage_record_count: row.get(1)?,
                    successful_request_count: row.get(2)?,
                    failed_request_count: row.get(3)?,
                    input_tokens: row.get(4)?,
                    cached_input_tokens: row.get(5)?,
                    cache_creation_input_tokens: row.get(6)?,
                    output_tokens: row.get(7)?,
                    priced_record_count: row.get(8)?,
                    unpriced_record_count: row.get(9)?,
                    estimated_cost_nano_usd: row.get(10)?,
                    latency_observation_count: row.get(11)?,
                    total_latency_ms: row.get(12)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

fn aggregate_date(
    transaction: &Transaction<'_>,
    target: Target,
    date: &str,
    aggregate: &mut RollupAccumulator,
) -> Result<(u64, u64), NativeUsageError> {
    let mut request_count = 0_u64;
    {
        let mut statement = transaction
            .prepare(
                "SELECT request.outcome, request.input_tokens,
                        request.cached_input_tokens, request.cache_creation_input_tokens,
                        request.output_tokens, request.latency_ms,
                        pricing.estimated_cost_nano_usd
                 FROM request_records request
                 LEFT JOIN pricing_snapshots pricing ON pricing.request_record_id = request.id
                 WHERE request.target = ?1
                   AND date(request.finished_at_unix_ms / 1000, 'unixepoch', 'localtime') = ?2",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map(params![target.as_str(), date], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, Option<u64>>(6)?,
                ))
            })
            .map_err(sql_error)?;
        for row in rows {
            let (outcome, input, cached, creation, output, latency, cost) =
                row.map_err(sql_error)?;
            request_count = checked_add(request_count, 1)?;
            aggregate.request_record_count = checked_add(aggregate.request_record_count, 1)?;
            if outcome == "success" {
                aggregate.successful_request_count =
                    checked_add(aggregate.successful_request_count, 1)?;
            } else {
                aggregate.failed_request_count = checked_add(aggregate.failed_request_count, 1)?;
            }
            add_usage(aggregate, input, cached, creation, output)?;
            add_pricing(aggregate, cost)?;
            aggregate.latency_observation_count =
                checked_add(aggregate.latency_observation_count, 1)?;
            aggregate.total_latency_ms = checked_add(aggregate.total_latency_ms, latency)?;
        }
    }
    let mut native_count = 0_u64;
    {
        let mut statement = transaction
            .prepare(
                "SELECT native.input_tokens, native.cached_input_tokens,
                        native.cache_creation_input_tokens, native.output_tokens,
                        pricing.estimated_cost_nano_usd
                 FROM native_usage_records native
                 LEFT JOIN native_usage_pricing_snapshots pricing
                   ON pricing.native_usage_record_id = native.id
                 WHERE native.target = ?1
                   AND date(native.observed_at_unix_ms / 1000, 'unixepoch', 'localtime') = ?2",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map(params![target.as_str(), date], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, Option<u64>>(4)?,
                ))
            })
            .map_err(sql_error)?;
        for row in rows {
            let (input, cached, creation, output, cost) = row.map_err(sql_error)?;
            native_count = checked_add(native_count, 1)?;
            aggregate.native_usage_record_count =
                checked_add(aggregate.native_usage_record_count, 1)?;
            add_usage(aggregate, input, cached, creation, output)?;
            add_pricing(aggregate, cost)?;
        }
    }
    Ok((request_count, native_count))
}

fn add_usage(
    aggregate: &mut RollupAccumulator,
    input: u64,
    cached: u64,
    creation: u64,
    output: u64,
) -> Result<(), NativeUsageError> {
    aggregate.input_tokens = checked_add(aggregate.input_tokens, input)?;
    aggregate.cached_input_tokens = checked_add(aggregate.cached_input_tokens, cached)?;
    aggregate.cache_creation_input_tokens =
        checked_add(aggregate.cache_creation_input_tokens, creation)?;
    aggregate.output_tokens = checked_add(aggregate.output_tokens, output)?;
    Ok(())
}

fn add_pricing(
    aggregate: &mut RollupAccumulator,
    cost: Option<u64>,
) -> Result<(), NativeUsageError> {
    if let Some(cost) = cost {
        aggregate.priced_record_count = checked_add(aggregate.priced_record_count, 1)?;
        aggregate.estimated_cost_nano_usd = checked_add(aggregate.estimated_cost_nano_usd, cost)?;
    } else {
        aggregate.unpriced_record_count = checked_add(aggregate.unpriced_record_count, 1)?;
    }
    Ok(())
}

fn write_rollup(
    transaction: &Transaction<'_>,
    target: Target,
    date: &str,
    value: &RollupAccumulator,
) -> Result<(), NativeUsageError> {
    transaction
        .execute(
            "INSERT INTO daily_usage_rollups
               (target, local_date, request_record_count, native_usage_record_count,
                successful_request_count, failed_request_count, input_tokens,
                cached_input_tokens, cache_creation_input_tokens, output_tokens,
                priced_record_count, unpriced_record_count, estimated_cost_nano_usd,
                latency_observation_count, total_latency_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(target, local_date) DO UPDATE SET
               request_record_count = excluded.request_record_count,
               native_usage_record_count = excluded.native_usage_record_count,
               successful_request_count = excluded.successful_request_count,
               failed_request_count = excluded.failed_request_count,
               input_tokens = excluded.input_tokens,
               cached_input_tokens = excluded.cached_input_tokens,
               cache_creation_input_tokens = excluded.cache_creation_input_tokens,
               output_tokens = excluded.output_tokens,
               priced_record_count = excluded.priced_record_count,
               unpriced_record_count = excluded.unpriced_record_count,
               estimated_cost_nano_usd = excluded.estimated_cost_nano_usd,
               latency_observation_count = excluded.latency_observation_count,
               total_latency_ms = excluded.total_latency_ms",
            params![
                target.as_str(),
                date,
                value.request_record_count,
                value.native_usage_record_count,
                value.successful_request_count,
                value.failed_request_count,
                value.input_tokens,
                value.cached_input_tokens,
                value.cache_creation_input_tokens,
                value.output_tokens,
                value.priced_record_count,
                value.unpriced_record_count,
                value.estimated_cost_nano_usd,
                value.latency_observation_count,
                value.total_latency_ms,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn count(transaction: &Transaction<'_>, table: &str) -> Result<u64, NativeUsageError> {
    let sql = match table {
        "request_records" => "SELECT COUNT(*) FROM request_records",
        "native_usage_records" => "SELECT COUNT(*) FROM native_usage_records",
        "daily_usage_rollups" => "SELECT COUNT(*) FROM daily_usage_rollups",
        "native_usage_import_cursors" => "SELECT COUNT(*) FROM native_usage_import_cursors",
        _ => return Err(NativeUsageError::Unavailable),
    };
    transaction
        .query_row(sql, [], |row| row.get(0))
        .map_err(sql_error)
}

fn usage_view(input: u64, cached: u64, creation: u64, output: u64) -> RequestUsageView {
    RequestUsageView {
        input_tokens: input,
        cached_input_tokens: cached,
        cache_creation_input_tokens: creation,
        output_tokens: output,
    }
}

fn checked_add(left: u64, right: u64) -> Result<u64, NativeUsageError> {
    left.checked_add(right).ok_or(NativeUsageError::Unavailable)
}

fn encode_cursor(target: Target, key: ActivityKey) -> String {
    URL_SAFE_NO_PAD.encode(format!(
        "{CURSOR_VERSION}|{}|{}|{}|{}",
        target.as_str(),
        key.0,
        key.1,
        key.2
    ))
}

fn decode_cursor(target: Target, cursor: &str) -> Result<ActivityKey, NativeUsageError> {
    let decoded = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| NativeUsageError::InvalidRequest)?;
    let decoded = std::str::from_utf8(&decoded).map_err(|_| NativeUsageError::InvalidRequest)?;
    let fields = decoded.split('|').collect::<Vec<_>>();
    if fields.len() != 5 || fields[0] != CURSOR_VERSION || fields[1] != target.as_str() {
        return Err(NativeUsageError::InvalidRequest);
    }
    let key = ActivityKey(
        fields[2]
            .parse()
            .map_err(|_| NativeUsageError::InvalidRequest)?,
        fields[3]
            .parse()
            .map_err(|_| NativeUsageError::InvalidRequest)?,
        fields[4]
            .parse()
            .map_err(|_| NativeUsageError::InvalidRequest)?,
    );
    if encode_cursor(target, key) != cursor {
        return Err(NativeUsageError::InvalidRequest);
    }
    Ok(key)
}

fn sql_error(_error: tokio_rusqlite::rusqlite::Error) -> NativeUsageError {
    NativeUsageError::Unavailable
}
