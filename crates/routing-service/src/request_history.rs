use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt;

use serde::Deserialize;
use uuid::Uuid;

use crate::control::protocol::{ProviderProtocol, RequestRecordOutcome, Target};

const PARTS_PER_MILLION: u128 = 1_000_000;
const PRICE_DENOMINATOR: u128 = 1_000_000 * PARTS_PER_MILLION;
const RELEASE_PRICING_CATALOG: &str = include_str!("request_history/pricing-catalog.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RequestUsage {
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) cache_creation_input_tokens: u64,
    pub(crate) output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordedProvider {
    pub(crate) id: Uuid,
    pub(crate) name: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RequestRecordCompletion {
    pub(crate) id: Uuid,
    pub(crate) target: Target,
    pub(crate) plan_id: Uuid,
    pub(crate) plan_epoch: Uuid,
    pub(crate) provider: Option<RecordedProvider>,
    pub(crate) model: String,
    pub(crate) protocol: ProviderProtocol,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) finished_at_unix_ms: u64,
    pub(crate) outcome: RequestRecordOutcome,
    pub(crate) http_status: Option<u16>,
    pub(crate) usage: Option<RequestUsage>,
    pub(crate) error_payload: Option<Vec<u8>>,
    pub(crate) error_payload_truncated: bool,
}

impl fmt::Debug for RequestRecordCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestRecordCompletion")
            .field("id", &self.id)
            .field("target", &self.target)
            .field("plan_id", &self.plan_id)
            .field("plan_epoch", &self.plan_epoch)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("protocol", &self.protocol)
            .field("started_at_unix_ms", &self.started_at_unix_ms)
            .field("finished_at_unix_ms", &self.finished_at_unix_ms)
            .field("outcome", &self.outcome)
            .field("http_status", &self.http_status)
            .field("usage", &self.usage)
            .field(
                "error_payload",
                &self.error_payload.as_ref().map(|payload| payload.len()),
            )
            .field("error_payload_truncated", &self.error_payload_truncated)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PricingSnapshot {
    pub(crate) catalog_version: String,
    pub(crate) source: String,
    pub(crate) source_model: String,
    pub(crate) input_nano_usd_per_million: u64,
    pub(crate) output_nano_usd_per_million: u64,
    pub(crate) cache_read_multiplier_ppm: u64,
    pub(crate) cache_creation_multiplier_ppm: u64,
    pub(crate) priced_at_unix_ms: u64,
    pub(crate) estimated_cost_nano_usd: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum PricingError {
    #[error("pricing catalog is invalid")]
    InvalidCatalog,
    #[error("pricing calculation overflowed")]
    ArithmeticOverflow,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RequestRecordStoreError {
    #[error("request record store is unavailable")]
    Unavailable,
    #[error("request record storage failed")]
    Sqlite(#[from] tokio_rusqlite::rusqlite::Error),
    #[error("request record is invalid")]
    InvalidRecord,
    #[error("request pricing calculation overflowed")]
    PricingOverflow,
}

#[derive(Clone)]
pub(crate) struct PricingCatalog {
    version: String,
    source: String,
    models: BTreeMap<String, CatalogPrice>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogPrice {
    model: String,
    input_nano_usd_per_million: u64,
    output_nano_usd_per_million: u64,
    cache_read_multiplier_ppm: u64,
    cache_creation_multiplier_ppm: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogDocument {
    version: String,
    source: String,
    models: Vec<CatalogPrice>,
}

impl PricingCatalog {
    pub(crate) fn release_pinned() -> Result<Self, PricingError> {
        Self::from_json(RELEASE_PRICING_CATALOG)
    }

    pub(crate) fn from_json(json: &str) -> Result<Self, PricingError> {
        let document: CatalogDocument =
            serde_json::from_str(json).map_err(|_| PricingError::InvalidCatalog)?;
        if document.version.trim().is_empty() || document.source.trim().is_empty() {
            return Err(PricingError::InvalidCatalog);
        }
        let mut models = BTreeMap::new();
        for price in document.models {
            if price.model.trim().is_empty()
                || price.input_nano_usd_per_million > i64::MAX as u64
                || price.output_nano_usd_per_million > i64::MAX as u64
                || price.cache_read_multiplier_ppm > i64::MAX as u64
                || price.cache_creation_multiplier_ppm > i64::MAX as u64
            {
                return Err(PricingError::InvalidCatalog);
            }
            match models.entry(price.model.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(price);
                }
                Entry::Occupied(_) => return Err(PricingError::InvalidCatalog),
            }
        }
        Ok(Self {
            version: document.version,
            source: document.source,
            models,
        })
    }

    pub(crate) fn price(
        &self,
        model: &str,
        usage: RequestUsage,
        priced_at_unix_ms: u64,
    ) -> Result<Option<PricingSnapshot>, PricingError> {
        let Some(price) = self.models.get(model) else {
            return Ok(None);
        };
        let mut numerator = 0_u128;
        for (tokens, unit_price, multiplier) in [
            (
                usage.input_tokens,
                price.input_nano_usd_per_million,
                1_000_000,
            ),
            (
                usage.cached_input_tokens,
                price.input_nano_usd_per_million,
                price.cache_read_multiplier_ppm,
            ),
            (
                usage.cache_creation_input_tokens,
                price.input_nano_usd_per_million,
                price.cache_creation_multiplier_ppm,
            ),
            (
                usage.output_tokens,
                price.output_nano_usd_per_million,
                1_000_000,
            ),
        ] {
            let component = u128::from(tokens)
                .checked_mul(u128::from(unit_price))
                .and_then(|value| value.checked_mul(u128::from(multiplier)))
                .ok_or(PricingError::ArithmeticOverflow)?;
            numerator = numerator
                .checked_add(component)
                .ok_or(PricingError::ArithmeticOverflow)?;
        }
        let estimate = numerator
            .checked_add(PRICE_DENOMINATOR / 2)
            .ok_or(PricingError::ArithmeticOverflow)?
            / PRICE_DENOMINATOR;
        if estimate == 0 {
            return Ok(None);
        }
        let estimated_cost_nano_usd =
            u64::try_from(estimate).map_err(|_| PricingError::ArithmeticOverflow)?;
        if estimated_cost_nano_usd > i64::MAX as u64 || priced_at_unix_ms > i64::MAX as u64 {
            return Err(PricingError::ArithmeticOverflow);
        }
        Ok(Some(PricingSnapshot {
            catalog_version: self.version.clone(),
            source: self.source.clone(),
            source_model: price.model.clone(),
            input_nano_usd_per_million: price.input_nano_usd_per_million,
            output_nano_usd_per_million: price.output_nano_usd_per_million,
            cache_read_multiplier_ppm: price.cache_read_multiplier_ppm,
            cache_creation_multiplier_ppm: price.cache_creation_multiplier_ppm,
            priced_at_unix_ms,
            estimated_cost_nano_usd,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::{
        control::protocol::{ProviderProtocol, RequestRecordOutcome, Target},
        home::MuxviaHome,
        state::StateStore,
    };
    use uuid::Uuid;

    const CATALOG: &str = r#"{
      "version": "test-v1",
      "source": "models.dev",
      "models": [
        {
          "model": "priced-model",
          "inputNanoUsdPerMillion": 2000000000,
          "outputNanoUsdPerMillion": 10000000000,
          "cacheReadMultiplierPpm": 100000,
          "cacheCreationMultiplierPpm": 1250000
        },
        {
          "model": "half-up-model",
          "inputNanoUsdPerMillion": 1,
          "outputNanoUsdPerMillion": 0,
          "cacheReadMultiplierPpm": 0,
          "cacheCreationMultiplierPpm": 0
        },
        {
          "model": "zero-model",
          "inputNanoUsdPerMillion": 0,
          "outputNanoUsdPerMillion": 0,
          "cacheReadMultiplierPpm": 0,
          "cacheCreationMultiplierPpm": 0
        }
      ]
    }"#;

    #[test]
    fn fixed_point_pricing_is_exact_half_up_and_model_bound() {
        let catalog = PricingCatalog::from_json(CATALOG).unwrap();
        let usage = RequestUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            output_tokens: 1_000_000,
        };
        let priced = catalog
            .price("priced-model", usage, 1_787_241_600_123)
            .unwrap()
            .unwrap();
        assert_eq!(priced.estimated_cost_nano_usd, 14_700_000_000);
        assert_eq!(priced.catalog_version, "test-v1");
        assert_eq!(priced.source_model, "priced-model");

        let rounded = catalog
            .price(
                "half-up-model",
                RequestUsage {
                    input_tokens: 500_000,
                    cached_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    output_tokens: 0,
                },
                1,
            )
            .unwrap()
            .unwrap();
        assert_eq!(rounded.estimated_cost_nano_usd, 1);

        assert!(catalog.price("model-alias", usage, 1).unwrap().is_none());
        assert!(catalog.price("zero-model", usage, 1).unwrap().is_none());
    }

    #[test]
    fn fixed_point_pricing_rejects_overflow_without_saturation() {
        let catalog = PricingCatalog::from_json(&format!(
            r#"{{
              "version": "overflow-v1",
              "source": "test",
              "models": [{{
                "model": "overflow-model",
                "inputNanoUsdPerMillion": {},
                "outputNanoUsdPerMillion": 0,
                "cacheReadMultiplierPpm": 1000000,
                "cacheCreationMultiplierPpm": 1000000
              }}]
            }}"#,
            i64::MAX
        ))
        .unwrap();
        let result = catalog.price(
            "overflow-model",
            RequestUsage {
                input_tokens: u64::MAX,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                output_tokens: 0,
            },
            1,
        );
        assert!(matches!(result, Err(PricingError::ArithmeticOverflow)));
    }

    #[test]
    fn release_catalog_is_auditable_exact_and_offline() {
        let catalog = PricingCatalog::release_pinned().unwrap();
        let usage = RequestUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            output_tokens: 1_000_000,
        };
        let openai = catalog.price("gpt-5.6", usage, 1).unwrap().unwrap();
        assert_eq!(openai.catalog_version, "models.dev-d2ec701bace7");
        assert_eq!(openai.input_nano_usd_per_million, 5_000_000_000);
        assert_eq!(openai.output_nano_usd_per_million, 30_000_000_000);
        assert_eq!(openai.estimated_cost_nano_usd, 41_750_000_000);

        let claude = catalog
            .price("claude-sonnet-4-5", usage, 1)
            .unwrap()
            .unwrap();
        assert_eq!(claude.input_nano_usd_per_million, 3_000_000_000);
        assert_eq!(claude.output_nano_usd_per_million, 15_000_000_000);
        assert_eq!(claude.estimated_cost_nano_usd, 22_050_000_000);
        assert!(catalog.price("GPT-5.6", usage, 1).unwrap().is_none());
    }

    fn completion(id: u128, model: &str, usage: RequestUsage) -> RequestRecordCompletion {
        RequestRecordCompletion {
            id: Uuid::from_u128(id),
            target: Target::Codex,
            plan_id: Uuid::from_u128(201),
            plan_epoch: Uuid::from_u128(202),
            provider: Some(RecordedProvider {
                id: Uuid::from_u128(203),
                name: "Recorded Provider".into(),
            }),
            model: model.into(),
            protocol: ProviderProtocol::OpenaiResponses,
            started_at_unix_ms: 1_000,
            finished_at_unix_ms: 1_123,
            outcome: RequestRecordOutcome::Success,
            http_status: Some(200),
            usage: Some(usage),
            error_payload: None,
            error_payload_truncated: false,
        }
    }

    async fn store_fixture(label: &str) -> (std::path::PathBuf, MuxviaHome, StateStore) {
        let root = std::env::temp_dir().join(format!("muxvia-{label}-{}", Uuid::new_v4()));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = StateStore::open(&home).await.unwrap();
        (root, home, store)
    }

    #[tokio::test]
    async fn request_and_nonzero_pricing_snapshot_insert_atomically() {
        let (root, home, store) = store_fixture("priced-request").await;
        let catalog = PricingCatalog::from_json(CATALOG).unwrap();
        let usage = RequestUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 1_000_000,
        };
        store
            .insert_request_record(completion(211, "priced-model", usage), &catalog)
            .await
            .unwrap();

        let database = tokio_rusqlite::Connection::open(home.database_path())
            .await
            .unwrap();
        let stored: (u64, u64, u64, String) = database
            .call(|connection| {
                connection.query_row(
                    "SELECT COUNT(*), pricing.estimated_cost_nano_usd,
                            request.latency_ms, pricing.catalog_version
                     FROM request_records request
                     JOIN pricing_snapshots pricing ON pricing.request_record_id = request.id",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
            })
            .await
            .unwrap();
        assert_eq!(stored, (1, 12_000_000_000, 123, "test-v1".into()));
        drop(database);
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn backfill_prices_only_unpriced_exact_models_once() {
        let (root, home, store) = store_fixture("pricing-backfill").await;
        let initial = PricingCatalog::from_json(CATALOG).unwrap();
        let usage = RequestUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
        };
        store
            .insert_request_record(completion(221, "later-model", usage), &initial)
            .await
            .unwrap();
        store
            .insert_request_record(completion(222, "zero-model", usage), &initial)
            .await
            .unwrap();
        let later = PricingCatalog::from_json(
            r#"{
              "version": "later-v1", "source": "models.dev", "models": [{
                "model": "later-model",
                "inputNanoUsdPerMillion": 3000000000,
                "outputNanoUsdPerMillion": 12000000000,
                "cacheReadMultiplierPpm": 100000,
                "cacheCreationMultiplierPpm": 1250000
              }]
            }"#,
        )
        .unwrap();
        assert_eq!(
            store.backfill_request_pricing(&later, 2_000).await.unwrap(),
            1
        );

        let changed = PricingCatalog::from_json(
            r#"{
              "version": "later-v2", "source": "models.dev", "models": [{
                "model": "later-model",
                "inputNanoUsdPerMillion": 9000000000,
                "outputNanoUsdPerMillion": 9000000000,
                "cacheReadMultiplierPpm": 1000000,
                "cacheCreationMultiplierPpm": 1000000
              }]
            }"#,
        )
        .unwrap();
        assert_eq!(
            store
                .backfill_request_pricing(&changed, 3_000)
                .await
                .unwrap(),
            0
        );

        let database = tokio_rusqlite::Connection::open(home.database_path())
            .await
            .unwrap();
        let frozen: (u64, String, u64) = database
            .call(|connection| {
                connection.query_row(
                    "SELECT COUNT(*), catalog_version, estimated_cost_nano_usd
                     FROM pricing_snapshots",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
            })
            .await
            .unwrap();
        assert_eq!(frozen, (1, "later-v1".into(), 3_000_000_000));
        drop(database);
        drop(store);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn pricing_overflow_fails_without_a_partial_request_record() {
        let (root, home, store) = store_fixture("pricing-overflow").await;
        let catalog = PricingCatalog::from_json(&format!(
            r#"{{
              "version": "overflow-v1", "source": "test", "models": [{{
                "model": "overflow-model",
                "inputNanoUsdPerMillion": {},
                "outputNanoUsdPerMillion": 0,
                "cacheReadMultiplierPpm": 1000000,
                "cacheCreationMultiplierPpm": 1000000
              }}]
            }}"#,
            i64::MAX
        ))
        .unwrap();
        let result = store
            .insert_request_record(
                completion(
                    231,
                    "overflow-model",
                    RequestUsage {
                        input_tokens: u64::MAX,
                        cached_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        output_tokens: 0,
                    },
                ),
                &catalog,
            )
            .await;
        assert!(result.is_err(), "accepted an overflowing request estimate");
        let database = tokio_rusqlite::Connection::open(home.database_path())
            .await
            .unwrap();
        let count: u64 = database
            .call(|connection| {
                connection.query_row("SELECT COUNT(*) FROM request_records", [], |row| row.get(0))
            })
            .await
            .unwrap();
        assert_eq!(count, 0);
        drop(database);
        drop(store);
        let _ = fs::remove_dir_all(root);
    }
}
