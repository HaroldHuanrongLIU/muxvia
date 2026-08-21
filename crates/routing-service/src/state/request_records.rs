use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio_rusqlite::rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    control::protocol::{
        PricingSnapshotView, ProviderProtocol, RequestRecordDetail, RequestRecordOutcome,
        RequestRecordPage, RequestRecordSummary, RequestUsageView, Target,
    },
    request_history::{
        PricingCatalog, PricingError, PricingSnapshot, RequestHistoryError,
        RequestRecordCompletion, RequestRecordStoreError, RequestUsage,
    },
};

use super::StateStore;

const MAX_ERROR_PAYLOAD_BYTES: usize = 65_536;
const REQUEST_HISTORY_CURSOR_VERSION: &str = "request-record-v1";

impl StateStore {
    pub(crate) async fn record_request_completion(
        &self,
        completion: RequestRecordCompletion,
    ) -> Result<(), RequestRecordStoreError> {
        validate_completion(&completion)?;
        self.connection
            .call(move |connection| -> Result<(), RequestRecordStoreError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let catalog_json = transaction.query_row(
                    "SELECT catalog_json FROM pricing_catalog_state WHERE singleton = 1",
                    [],
                    |row| row.get::<_, String>(0),
                )?;
                let catalog =
                    PricingCatalog::from_json(&catalog_json).map_err(map_pricing_error)?;
                let pricing = match completion.usage {
                    Some(usage) => catalog
                        .price(&completion.model, usage, completion.finished_at_unix_ms)
                        .map_err(map_pricing_error)?,
                    None => None,
                };
                insert_completion(&transaction, &completion)?;
                if let Some(snapshot) = pricing.as_ref() {
                    insert_pricing_snapshot(&transaction, completion.id, snapshot)?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(map_call_error)
    }

    #[cfg(test)]
    pub(crate) async fn insert_request_record(
        &self,
        completion: RequestRecordCompletion,
        catalog: &PricingCatalog,
    ) -> Result<(), RequestRecordStoreError> {
        validate_completion(&completion)?;
        let pricing = match completion.usage {
            Some(usage) => catalog
                .price(&completion.model, usage, completion.finished_at_unix_ms)
                .map_err(map_pricing_error)?,
            None => None,
        };
        self.connection
            .call(move |connection| -> Result<(), RequestRecordStoreError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                insert_completion(&transaction, &completion)?;
                if let Some(snapshot) = pricing.as_ref() {
                    insert_pricing_snapshot(&transaction, completion.id, snapshot)?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(map_call_error)
    }

    // T14 catalog replacement consumes this one-time backfill seam.
    #[allow(dead_code)]
    pub(crate) async fn backfill_request_pricing(
        &self,
        catalog: &PricingCatalog,
        priced_at_unix_ms: u64,
    ) -> Result<u64, RequestRecordStoreError> {
        if priced_at_unix_ms > i64::MAX as u64 {
            return Err(RequestRecordStoreError::PricingOverflow);
        }
        let catalog = catalog.clone();
        self.connection
            .call(move |connection| -> Result<u64, RequestRecordStoreError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let unpriced = {
                    let mut statement = transaction.prepare(
                        "SELECT request.id, request.model, request.input_tokens,
                                request.cached_input_tokens,
                                request.cache_creation_input_tokens, request.output_tokens
                         FROM request_records request
                         LEFT JOIN pricing_snapshots pricing
                           ON pricing.request_record_id = request.id
                         WHERE request.usage_observed = 1
                           AND pricing.request_record_id IS NULL
                         ORDER BY request.sequence",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, u64>(2)?,
                                row.get::<_, u64>(3)?,
                                row.get::<_, u64>(4)?,
                                row.get::<_, u64>(5)?,
                            ))
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let mut inserted = 0_u64;
                for (id, model, input, cached, cache_creation, output) in unpriced {
                    let usage = RequestUsage {
                        input_tokens: input,
                        cached_input_tokens: cached,
                        cache_creation_input_tokens: cache_creation,
                        output_tokens: output,
                    };
                    let Some(snapshot) = catalog
                        .price(&model, usage, priced_at_unix_ms)
                        .map_err(map_pricing_error)?
                    else {
                        continue;
                    };
                    let id = uuid::Uuid::parse_str(&id)
                        .map_err(|_| RequestRecordStoreError::InvalidRecord)?;
                    insert_pricing_snapshot(&transaction, id, &snapshot)?;
                    inserted = inserted
                        .checked_add(1)
                        .ok_or(RequestRecordStoreError::PricingOverflow)?;
                }
                transaction.commit()?;
                Ok(inserted)
            })
            .await
            .map_err(map_call_error)
    }

    pub(crate) async fn list_request_records(
        &self,
        target: Target,
        before_cursor: Option<&str>,
        limit: u16,
    ) -> Result<RequestRecordPage, RequestHistoryError> {
        if !(1..=100).contains(&limit) {
            return Err(RequestHistoryError::InvalidCursor);
        }
        let before_sequence = before_cursor
            .map(|cursor| decode_cursor(target, cursor))
            .transpose()?;
        let query_limit = u64::from(limit) + 1;
        let target_name = target.as_str().to_owned();
        let rows = self
            .connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT request.sequence, request.id, request.plan_id, request.plan_epoch,
                            request.provider_id, request.provider_name, request.model,
                            request.protocol, request.started_at_unix_ms,
                            request.finished_at_unix_ms, request.latency_ms, request.outcome,
                            request.http_status, request.usage_observed, request.input_tokens,
                            request.cached_input_tokens,
                            request.cache_creation_input_tokens, request.output_tokens,
                            request.error_payload IS NOT NULL,
                            request.error_payload_truncated,
                            pricing.estimated_cost_nano_usd
                     FROM request_records request
                     LEFT JOIN pricing_snapshots pricing
                       ON pricing.request_record_id = request.id
                     WHERE request.target = ?1
                       AND (?2 IS NULL OR request.sequence < ?2)
                     ORDER BY request.sequence DESC
                     LIMIT ?3",
                )?;
                statement
                    .query_map(params![target_name, before_sequence, query_limit], |row| {
                        StoredRequestRecord::from_row(row)
                    })?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(|_| RequestHistoryError::Unavailable)?;
        let mut rows = rows;
        let has_more = rows.len() > usize::from(limit);
        if has_more {
            rows.pop();
        }
        let next_cursor = has_more
            .then(|| rows.last().map(|row| encode_cursor(target, row.sequence)))
            .flatten();
        let records = rows
            .into_iter()
            .map(StoredRequestRecord::into_summary)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RequestRecordPage {
            target,
            records,
            next_cursor,
        })
    }

    pub(crate) async fn inspect_request_record(
        &self,
        target: Target,
        record_id: uuid::Uuid,
    ) -> Result<RequestRecordDetail, RequestHistoryError> {
        let target_name = target.as_str().to_owned();
        let record_id = record_id.to_string();
        let stored = self
            .connection
            .call(move |connection| {
                connection
                    .query_row(
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
                                pricing.estimated_cost_nano_usd,
                                pricing.catalog_version, pricing.source, pricing.source_model,
                                pricing.input_nano_usd_per_million,
                                pricing.output_nano_usd_per_million,
                                pricing.cache_read_multiplier_ppm,
                                pricing.cache_creation_multiplier_ppm,
                                pricing.priced_at_unix_ms, request.error_payload
                         FROM request_records request
                         LEFT JOIN pricing_snapshots pricing
                           ON pricing.request_record_id = request.id
                         WHERE request.target = ?1
                           AND request.id = ?2
                           AND request.outcome != 'success'",
                        params![target_name, record_id],
                        |row| {
                            Ok((
                                StoredRequestRecord::from_row(row)?,
                                StoredPricingSnapshot::from_row(row)?,
                                row.get::<_, Option<Vec<u8>>>(29)?,
                            ))
                        },
                    )
                    .optional()
            })
            .await
            .map_err(|_| RequestHistoryError::Unavailable)?
            .ok_or(RequestHistoryError::NotFound)?;
        let (record, pricing, error_payload) = stored;
        let pricing_snapshot = pricing.into_view(record.estimated_cost_nano_usd)?;
        Ok(RequestRecordDetail {
            target,
            record: record.into_summary()?,
            pricing_snapshot,
            error_payload: error_payload
                .as_deref()
                .map(String::from_utf8_lossy)
                .map(std::borrow::Cow::into_owned),
            error_payload_sensitive: error_payload.is_some(),
        })
    }
}

pub(super) struct StoredRequestRecord {
    pub(super) sequence: u64,
    id: String,
    plan_id: String,
    plan_epoch: String,
    provider_id: Option<String>,
    provider_name: Option<String>,
    model: String,
    protocol: String,
    started_at_unix_ms: u64,
    finished_at_unix_ms: u64,
    latency_ms: u64,
    outcome: String,
    http_status: Option<u16>,
    usage_observed: bool,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    output_tokens: u64,
    has_error_payload: bool,
    error_payload_truncated: bool,
    estimated_cost_nano_usd: Option<u64>,
}

impl StoredRequestRecord {
    pub(super) fn from_row(
        row: &tokio_rusqlite::rusqlite::Row<'_>,
    ) -> tokio_rusqlite::rusqlite::Result<Self> {
        Ok(Self {
            sequence: row.get(0)?,
            id: row.get(1)?,
            plan_id: row.get(2)?,
            plan_epoch: row.get(3)?,
            provider_id: row.get(4)?,
            provider_name: row.get(5)?,
            model: row.get(6)?,
            protocol: row.get(7)?,
            started_at_unix_ms: row.get(8)?,
            finished_at_unix_ms: row.get(9)?,
            latency_ms: row.get(10)?,
            outcome: row.get(11)?,
            http_status: row.get(12)?,
            usage_observed: row.get(13)?,
            input_tokens: row.get(14)?,
            cached_input_tokens: row.get(15)?,
            cache_creation_input_tokens: row.get(16)?,
            output_tokens: row.get(17)?,
            has_error_payload: row.get(18)?,
            error_payload_truncated: row.get(19)?,
            estimated_cost_nano_usd: row.get(20)?,
        })
    }

    pub(super) fn into_summary(self) -> Result<RequestRecordSummary, RequestHistoryError> {
        Ok(RequestRecordSummary {
            id: parse_uuid(&self.id)?,
            plan_id: parse_uuid(&self.plan_id)?,
            plan_epoch: parse_uuid(&self.plan_epoch)?,
            provider_id: self.provider_id.as_deref().map(parse_uuid).transpose()?,
            provider_name: self.provider_name,
            model: self.model,
            protocol: parse_protocol(&self.protocol)?,
            started_at_unix_ms: self.started_at_unix_ms,
            finished_at_unix_ms: self.finished_at_unix_ms,
            latency_ms: self.latency_ms,
            outcome: parse_outcome(&self.outcome)?,
            http_status: self.http_status,
            usage: self.usage_observed.then_some(RequestUsageView {
                input_tokens: self.input_tokens,
                cached_input_tokens: self.cached_input_tokens,
                cache_creation_input_tokens: self.cache_creation_input_tokens,
                output_tokens: self.output_tokens,
            }),
            estimated_cost_nano_usd: self.estimated_cost_nano_usd,
            has_error_payload: self.has_error_payload,
            error_payload_truncated: self.error_payload_truncated,
        })
    }
}

struct StoredPricingSnapshot {
    catalog_version: Option<String>,
    source: Option<String>,
    source_model: Option<String>,
    input_nano_usd_per_million: Option<u64>,
    output_nano_usd_per_million: Option<u64>,
    cache_read_multiplier_ppm: Option<u64>,
    cache_creation_multiplier_ppm: Option<u64>,
    priced_at_unix_ms: Option<u64>,
}

impl StoredPricingSnapshot {
    fn from_row(row: &tokio_rusqlite::rusqlite::Row<'_>) -> tokio_rusqlite::rusqlite::Result<Self> {
        Ok(Self {
            catalog_version: row.get(21)?,
            source: row.get(22)?,
            source_model: row.get(23)?,
            input_nano_usd_per_million: row.get(24)?,
            output_nano_usd_per_million: row.get(25)?,
            cache_read_multiplier_ppm: row.get(26)?,
            cache_creation_multiplier_ppm: row.get(27)?,
            priced_at_unix_ms: row.get(28)?,
        })
    }

    fn into_view(
        self,
        estimated_cost_nano_usd: Option<u64>,
    ) -> Result<Option<PricingSnapshotView>, RequestHistoryError> {
        match (
            self.catalog_version,
            self.source,
            self.source_model,
            self.input_nano_usd_per_million,
            self.output_nano_usd_per_million,
            self.cache_read_multiplier_ppm,
            self.cache_creation_multiplier_ppm,
            self.priced_at_unix_ms,
            estimated_cost_nano_usd,
        ) {
            (None, None, None, None, None, None, None, None, None) => Ok(None),
            (
                Some(catalog_version),
                Some(source),
                Some(source_model),
                Some(input_nano_usd_per_million),
                Some(output_nano_usd_per_million),
                Some(cache_read_multiplier_ppm),
                Some(cache_creation_multiplier_ppm),
                Some(priced_at_unix_ms),
                Some(estimated_cost_nano_usd),
            ) => Ok(Some(PricingSnapshotView {
                catalog_version,
                source,
                source_model,
                input_nano_usd_per_million,
                output_nano_usd_per_million,
                cache_read_multiplier_ppm,
                cache_creation_multiplier_ppm,
                priced_at_unix_ms,
                estimated_cost_nano_usd,
            })),
            _ => Err(RequestHistoryError::Unavailable),
        }
    }
}

fn encode_cursor(target: Target, sequence: u64) -> String {
    URL_SAFE_NO_PAD.encode(format!(
        "{REQUEST_HISTORY_CURSOR_VERSION}|{}|{sequence}",
        target.as_str()
    ))
}

fn decode_cursor(target: Target, cursor: &str) -> Result<u64, RequestHistoryError> {
    let decoded = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| RequestHistoryError::InvalidCursor)?;
    let decoded = std::str::from_utf8(&decoded).map_err(|_| RequestHistoryError::InvalidCursor)?;
    let mut fields = decoded.split('|');
    let version = fields.next();
    let encoded_target = fields.next();
    let sequence = fields.next();
    if version != Some(REQUEST_HISTORY_CURSOR_VERSION)
        || encoded_target != Some(target.as_str())
        || fields.next().is_some()
    {
        return Err(RequestHistoryError::InvalidCursor);
    }
    let sequence = sequence
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or(RequestHistoryError::InvalidCursor)?;
    if encode_cursor(target, sequence) != cursor {
        return Err(RequestHistoryError::InvalidCursor);
    }
    Ok(sequence)
}

fn parse_uuid(value: &str) -> Result<uuid::Uuid, RequestHistoryError> {
    uuid::Uuid::parse_str(value).map_err(|_| RequestHistoryError::Unavailable)
}

fn parse_protocol(value: &str) -> Result<ProviderProtocol, RequestHistoryError> {
    match value {
        "openai-responses" => Ok(ProviderProtocol::OpenaiResponses),
        "anthropic-messages" => Ok(ProviderProtocol::AnthropicMessages),
        _ => Err(RequestHistoryError::Unavailable),
    }
}

fn parse_outcome(value: &str) -> Result<RequestRecordOutcome, RequestHistoryError> {
    match value {
        "success" => Ok(RequestRecordOutcome::Success),
        "upstream-error" => Ok(RequestRecordOutcome::UpstreamError),
        "semantic-error" => Ok(RequestRecordOutcome::SemanticError),
        "transport-error" => Ok(RequestRecordOutcome::TransportError),
        "route-unavailable" => Ok(RequestRecordOutcome::RouteUnavailable),
        "cancelled" => Ok(RequestRecordOutcome::Cancelled),
        "stream-error" => Ok(RequestRecordOutcome::StreamError),
        _ => Err(RequestHistoryError::Unavailable),
    }
}

fn validate_completion(
    completion: &RequestRecordCompletion,
) -> Result<(), RequestRecordStoreError> {
    let latency = completion
        .finished_at_unix_ms
        .checked_sub(completion.started_at_unix_ms)
        .ok_or(RequestRecordStoreError::InvalidRecord)?;
    if completion.started_at_unix_ms > i64::MAX as u64
        || completion.finished_at_unix_ms > i64::MAX as u64
        || latency > i64::MAX as u64
        || completion.model.is_empty()
        || completion
            .http_status
            .is_some_and(|status| !(100..=999).contains(&status))
        || completion
            .error_payload
            .as_ref()
            .is_some_and(|payload| payload.len() > MAX_ERROR_PAYLOAD_BYTES)
        || (completion.error_payload.is_some()
            && completion.outcome != RequestRecordOutcome::UpstreamError)
        || (completion.error_payload_truncated && completion.error_payload.is_none())
    {
        return Err(RequestRecordStoreError::InvalidRecord);
    }
    if let Some(usage) = completion.usage {
        for tokens in [
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.cache_creation_input_tokens,
            usage.output_tokens,
        ] {
            if tokens > i64::MAX as u64 {
                return Err(RequestRecordStoreError::PricingOverflow);
            }
        }
    }
    Ok(())
}

fn insert_completion(
    transaction: &Transaction<'_>,
    completion: &RequestRecordCompletion,
) -> Result<(), RequestRecordStoreError> {
    let (provider_id, provider_name) = completion
        .provider
        .as_ref()
        .map(|provider| (Some(provider.id.to_string()), Some(provider.name.as_str())))
        .unwrap_or((None, None));
    let usage = completion.usage.unwrap_or(RequestUsage {
        input_tokens: 0,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        output_tokens: 0,
    });
    let latency = completion.finished_at_unix_ms - completion.started_at_unix_ms;
    transaction.execute(
        "INSERT INTO request_records
           (id, target, plan_id, plan_epoch, provider_id, provider_name, model, protocol,
            started_at_unix_ms, finished_at_unix_ms, latency_ms, outcome, http_status,
            usage_observed, input_tokens, cached_input_tokens, cache_creation_input_tokens,
            output_tokens, error_payload, error_payload_truncated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            completion.id.to_string(),
            completion.target.as_str(),
            completion.plan_id.to_string(),
            completion.plan_epoch.to_string(),
            provider_id,
            provider_name,
            completion.model,
            protocol_name(completion.protocol),
            completion.started_at_unix_ms,
            completion.finished_at_unix_ms,
            latency,
            outcome_name(completion.outcome),
            completion.http_status,
            u8::from(completion.usage.is_some()),
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.cache_creation_input_tokens,
            usage.output_tokens,
            completion.error_payload,
            u8::from(completion.error_payload_truncated),
        ],
    )?;
    Ok(())
}

pub(super) fn insert_pricing_snapshot(
    transaction: &Transaction<'_>,
    request_record_id: uuid::Uuid,
    snapshot: &PricingSnapshot,
) -> Result<(), RequestRecordStoreError> {
    let existing: Option<u8> = transaction
        .query_row(
            "SELECT 1 FROM pricing_snapshots WHERE request_record_id = ?1",
            [request_record_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO pricing_snapshots
           (request_record_id, catalog_version, source, source_model,
            input_nano_usd_per_million, output_nano_usd_per_million,
            cache_read_multiplier_ppm, cache_creation_multiplier_ppm,
            priced_at_unix_ms, estimated_cost_nano_usd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            request_record_id.to_string(),
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
    )?;
    Ok(())
}

fn map_pricing_error(error: PricingError) -> RequestRecordStoreError {
    match error {
        PricingError::InvalidCatalog => RequestRecordStoreError::InvalidRecord,
        PricingError::ArithmeticOverflow => RequestRecordStoreError::PricingOverflow,
    }
}

fn map_call_error(
    error: tokio_rusqlite::Error<RequestRecordStoreError>,
) -> RequestRecordStoreError {
    match error {
        tokio_rusqlite::Error::ConnectionClosed => RequestRecordStoreError::Unavailable,
        tokio_rusqlite::Error::Error(error) => error,
        _ => RequestRecordStoreError::Unavailable,
    }
}

fn protocol_name(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::OpenaiResponses => "openai-responses",
        ProviderProtocol::AnthropicMessages => "anthropic-messages",
    }
}

fn outcome_name(outcome: RequestRecordOutcome) -> &'static str {
    match outcome {
        RequestRecordOutcome::Success => "success",
        RequestRecordOutcome::UpstreamError => "upstream-error",
        RequestRecordOutcome::SemanticError => "semantic-error",
        RequestRecordOutcome::TransportError => "transport-error",
        RequestRecordOutcome::RouteUnavailable => "route-unavailable",
        RequestRecordOutcome::Cancelled => "cancelled",
        RequestRecordOutcome::StreamError => "stream-error",
    }
}
