use tokio_rusqlite::rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    control::protocol::{ProviderProtocol, RequestRecordOutcome},
    request_history::{
        PricingCatalog, PricingError, PricingSnapshot, RequestRecordCompletion,
        RequestRecordStoreError, RequestUsage,
    },
};

use super::StateStore;

const MAX_ERROR_PAYLOAD_BYTES: usize = 65_536;

impl StateStore {
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

fn insert_pricing_snapshot(
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
