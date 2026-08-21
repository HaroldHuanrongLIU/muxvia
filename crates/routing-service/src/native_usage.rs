use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use ring::digest::{SHA256, digest};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    control::protocol::{
        NativeUsageRefresh, PricingCatalogUpdateOutcome, Target, UsageActivityPage,
        UsageClearOutcome, UsageRetentionOutcome,
    },
    home::MuxviaHome,
    request_history::{PricingCatalog, RequestUsage},
    state::StateStore,
};

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const MAX_NATIVE_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum NativeUsageError {
    #[error("native usage is unavailable")]
    Unavailable,
    #[error("native usage request is invalid")]
    InvalidRequest,
    #[error("pricing catalog candidate is invalid")]
    InvalidCatalog,
}

#[derive(Clone)]
pub(crate) struct NativeUsageService {
    user_home: PathBuf,
    store: Arc<StateStore>,
    client: reqwest::Client,
    catalog_url: String,
    scans: [Arc<Mutex<()>>; 2],
}

#[derive(Clone)]
pub(super) struct ParsedNativeFile {
    pub(super) source_fingerprint: String,
    pub(super) modified_unix_nanos: u64,
    pub(super) byte_length: u64,
    pub(super) completed_line_count: u64,
    pub(super) records: Vec<NativeUsageRecordInput>,
}

#[derive(Clone)]
pub(super) struct NativeUsageRecordInput {
    pub(super) line_number: u64,
    pub(super) source_record_fingerprint: String,
    pub(super) model: String,
    pub(super) observed_at_unix_ms: u64,
    pub(super) usage: RequestUsage,
}

impl NativeUsageService {
    pub(crate) fn new(home: &MuxviaHome, store: Arc<StateStore>) -> Result<Self, NativeUsageError> {
        Self::with_catalog_url(home, store, MODELS_DEV_URL)
    }

    fn with_catalog_url(
        home: &MuxviaHome,
        store: Arc<StateStore>,
        catalog_url: impl Into<String>,
    ) -> Result<Self, NativeUsageError> {
        let client = reqwest::Client::builder()
            .https_only(false)
            .build()
            .map_err(|_| NativeUsageError::Unavailable)?;
        Ok(Self {
            user_home: home.user_home().to_owned(),
            store,
            client,
            catalog_url: catalog_url.into(),
            scans: [Arc::new(Mutex::new(())), Arc::new(Mutex::new(()))],
        })
    }

    pub(crate) async fn scan(
        &self,
        target: Target,
    ) -> Result<NativeUsageRefresh, NativeUsageError> {
        let _guard = self.scans[target_index(target)].lock().await;
        let user_home = self.user_home.clone();
        let files = tokio::task::spawn_blocking(move || scan_files(&user_home, target))
            .await
            .map_err(|_| NativeUsageError::Unavailable)??;
        let scanned_files =
            u64::try_from(files.len()).map_err(|_| NativeUsageError::Unavailable)?;
        let imported_records = self.store.import_native_usage(target, files).await?;
        Ok(NativeUsageRefresh {
            target,
            imported_records,
            scanned_files,
        })
    }

    pub(crate) async fn list(
        &self,
        target: Target,
        before_cursor: Option<&str>,
        limit: u16,
    ) -> Result<UsageActivityPage, NativeUsageError> {
        self.store
            .list_usage_activity(target, before_cursor, limit)
            .await
    }

    pub(crate) async fn set_retention(
        &self,
        target: Target,
        detailed_retention_days: u16,
    ) -> Result<UsageRetentionOutcome, NativeUsageError> {
        if !(1..=3650).contains(&detailed_retention_days) {
            return Err(NativeUsageError::InvalidRequest);
        }
        self.store
            .apply_usage_retention(target, detailed_retention_days, unix_time_ms())
            .await
    }

    pub(crate) async fn clear(
        &self,
        target: Target,
    ) -> Result<UsageClearOutcome, NativeUsageError> {
        self.store.clear_usage(target).await
    }

    pub(crate) async fn update_catalog(
        &self,
        target: Target,
    ) -> Result<PricingCatalogUpdateOutcome, NativeUsageError> {
        let response = self
            .client
            .get(&self.catalog_url)
            .send()
            .await
            .map_err(|_| NativeUsageError::Unavailable)?;
        if !response.status().is_success() {
            return Err(NativeUsageError::Unavailable);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
        {
            return Err(NativeUsageError::InvalidCatalog);
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
            let chunk = chunk.map_err(|_| NativeUsageError::Unavailable)?;
            if bytes.len().saturating_add(chunk.len()) > MAX_CATALOG_BYTES {
                return Err(NativeUsageError::InvalidCatalog);
            }
            bytes.extend_from_slice(&chunk);
        }
        let catalog = normalize_models_dev(&bytes)?;
        self.store
            .replace_pricing_catalog(target, catalog, unix_time_ms())
            .await
    }
}

fn scan_files(user_home: &Path, target: Target) -> Result<Vec<ParsedNativeFile>, NativeUsageError> {
    let mut paths = Vec::new();
    match target {
        Target::Codex => {
            collect_jsonl(&user_home.join(".codex/sessions"), 4, &mut paths)?;
            collect_jsonl(&user_home.join(".codex/archived_sessions"), 1, &mut paths)?;
        }
        Target::Claude => collect_jsonl(&user_home.join(".claude/projects"), 6, &mut paths)?,
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| parse_file(user_home, target, &path))
        .collect()
}

fn collect_jsonl(
    directory: &Path,
    remaining_depth: usize,
    output: &mut Vec<PathBuf>,
) -> Result<(), NativeUsageError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(NativeUsageError::Unavailable),
    };
    for entry in entries {
        let entry = entry.map_err(|_| NativeUsageError::Unavailable)?;
        let file_type = entry
            .file_type()
            .map_err(|_| NativeUsageError::Unavailable)?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() && remaining_depth > 0 {
            collect_jsonl(&path, remaining_depth - 1, output)?;
        } else if file_type.is_file() && path.extension().is_some_and(|value| value == "jsonl") {
            output.push(path);
        }
    }
    Ok(())
}

fn parse_file(
    user_home: &Path,
    target: Target,
    path: &Path,
) -> Result<ParsedNativeFile, NativeUsageError> {
    let metadata = fs::metadata(path).map_err(|_| NativeUsageError::Unavailable)?;
    if metadata.len() > MAX_NATIVE_FILE_BYTES {
        return Err(NativeUsageError::Unavailable);
    }
    let bytes = fs::read(path).map_err(|_| NativeUsageError::Unavailable)?;
    let mut complete_length = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    if complete_length > 0 {
        let final_line_start = bytes[..complete_length - 1]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let final_line = &bytes[final_line_start..complete_length - 1];
        if !final_line.is_empty() && serde_json::from_slice::<Value>(final_line).is_err() {
            complete_length = final_line_start;
        }
    }
    let complete = &bytes[..complete_length];
    let completed_line_count = complete.iter().filter(|byte| **byte == b'\n').count() as u64;
    let relative = path.strip_prefix(user_home).unwrap_or(path);
    let initial_line = complete
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let source_fingerprint = fingerprint(&format!(
        "{}|{}|{}|{}",
        target.as_str(),
        relative.to_string_lossy(),
        file_identity(&metadata),
        hex_digest(initial_line),
    ));
    let modified_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
        .ok_or(NativeUsageError::Unavailable)?;
    let lines = complete.split(|byte| *byte == b'\n');
    let records = match target {
        Target::Codex => parse_codex_lines(lines),
        Target::Claude => parse_claude_lines(lines),
    };
    Ok(ParsedNativeFile {
        source_fingerprint,
        modified_unix_nanos,
        byte_length: metadata.len(),
        completed_line_count,
        records,
    })
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> String {
    "native-file".to_owned()
}

fn parse_codex_lines<'a>(lines: impl Iterator<Item = &'a [u8]>) -> Vec<NativeUsageRecordInput> {
    let mut session = None::<String>;
    let mut model = None::<String>;
    let mut cumulative = RequestUsage::zero();
    let mut records = Vec::new();
    for (index, line) in lines.enumerate() {
        let line_number = index as u64 + 1;
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                session = string_at(&value, &["/payload/id", "/payload/session_id", "/id"]);
            }
            Some("turn_context") => {
                model = string_at(&value, &["/payload/model", "/model"]);
            }
            Some("event_msg")
                if value.pointer("/payload/type").and_then(Value::as_str)
                    == Some("token_count") =>
            {
                let total = value
                    .pointer("/payload/info/total_token_usage")
                    .and_then(parse_codex_usage);
                let usage = value
                    .pointer("/payload/info/last_token_usage")
                    .and_then(parse_codex_usage)
                    .or_else(|| total.map(|total| total.positive_delta(cumulative)));
                if let Some(total) = total {
                    cumulative = total;
                }
                let Some((session, model, usage, observed_at_unix_ms)) =
                    session.as_ref().zip(model.as_ref()).zip(usage).and_then(
                        |((session, model), usage)| {
                            parse_event_time(&value).map(|time| (session, model, usage, time))
                        },
                    )
                else {
                    continue;
                };
                if usage.is_zero() {
                    continue;
                }
                records.push(NativeUsageRecordInput {
                    line_number,
                    source_record_fingerprint: fingerprint(&format!(
                        "codex|{session}|{line_number}"
                    )),
                    model: model.clone(),
                    observed_at_unix_ms,
                    usage,
                });
            }
            _ => {}
        }
    }
    records
}

fn parse_claude_lines<'a>(lines: impl Iterator<Item = &'a [u8]>) -> Vec<NativeUsageRecordInput> {
    let mut records = Vec::new();
    for (index, line) in lines.enumerate() {
        let line_number = index as u64 + 1;
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(session) = string_at(&value, &["/sessionId", "/session_id"]) else {
            continue;
        };
        let Some(message) = value.pointer("/message") else {
            continue;
        };
        let Some(message_id) = message.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(model) = message.get("model").and_then(Value::as_str) else {
            continue;
        };
        let Some(usage) = message.get("usage").and_then(parse_claude_usage) else {
            continue;
        };
        let Some(observed_at_unix_ms) = parse_event_time(&value) else {
            continue;
        };
        if usage.is_zero() {
            continue;
        }
        records.push(NativeUsageRecordInput {
            line_number,
            source_record_fingerprint: fingerprint(&format!(
                "claude|{session}|{message_id}|{}|{}|{}|{}",
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.cache_creation_input_tokens,
                usage.output_tokens
            )),
            model: model.to_owned(),
            observed_at_unix_ms,
            usage,
        });
    }
    records
}

fn string_at(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_codex_usage(value: &Value) -> Option<RequestUsage> {
    Some(RequestUsage {
        input_tokens: number(value, "input_tokens")?,
        cached_input_tokens: number_or_zero(value, "cached_input_tokens")?,
        cache_creation_input_tokens: 0,
        output_tokens: number(value, "output_tokens")?,
    })
}

fn parse_claude_usage(value: &Value) -> Option<RequestUsage> {
    Some(RequestUsage {
        input_tokens: number(value, "input_tokens")?,
        cached_input_tokens: number_or_zero(value, "cache_read_input_tokens")?,
        cache_creation_input_tokens: number_or_zero(value, "cache_creation_input_tokens")?,
        output_tokens: number(value, "output_tokens")?,
    })
}

fn number(value: &Value, field: &str) -> Option<u64> {
    value
        .get(field)?
        .as_u64()
        .filter(|value| *value <= i64::MAX as u64)
}

fn number_or_zero(value: &Value, field: &str) -> Option<u64> {
    value
        .get(field)
        .map(Value::as_u64)
        .unwrap_or(Some(0))
        .filter(|value| *value <= i64::MAX as u64)
}

fn parse_event_time(value: &Value) -> Option<u64> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms)
}

fn parse_rfc3339_ms(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return None;
    }
    let year = parse_digits(bytes.get(0..4)?)? as i64;
    let month = parse_digits(bytes.get(5..7)?)? as u32;
    let day = parse_digits(bytes.get(8..10)?)? as u32;
    if bytes.get(10) != Some(&b'T') && bytes.get(10) != Some(&b't') {
        return None;
    }
    let hour = parse_digits(bytes.get(11..13)?)? as u32;
    let minute = parse_digits(bytes.get(14..16)?)? as u32;
    let second = parse_digits(bytes.get(17..19)?)? as u32;
    if bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut cursor = 19;
    let mut millis = 0_u32;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == start {
            return None;
        }
        let fraction = &bytes[start..cursor];
        millis = u32::from(*fraction.first()?) * 100
            + u32::from(*fraction.get(1).unwrap_or(&b'0')) * 10
            + u32::from(*fraction.get(2).unwrap_or(&b'0'));
        millis = millis.checked_sub(u32::from(b'0') * 111)?;
    }
    let offset_seconds = match bytes.get(cursor)? {
        b'Z' | b'z' if cursor + 1 == bytes.len() => 0_i64,
        sign @ (b'+' | b'-') if cursor + 6 == bytes.len() => {
            let offset_hour = parse_digits(bytes.get(cursor + 1..cursor + 3)?)? as i64;
            let offset_minute = parse_digits(bytes.get(cursor + 4..cursor + 6)?)? as i64;
            if bytes.get(cursor + 3) != Some(&b':') || offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let seconds = offset_hour * 3600 + offset_minute * 60;
            if *sign == b'+' { seconds } else { -seconds }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour * 3600 + minute * 60 + second))?
        .checked_sub(offset_seconds)?;
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1000)?
        .checked_add(u64::from(millis))
}

fn parse_digits(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u64::from(*byte - b'0'))
    })
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn normalize_models_dev(bytes: &[u8]) -> Result<PricingCatalog, NativeUsageError> {
    let root: Value =
        serde_json::from_slice(bytes).map_err(|_| NativeUsageError::InvalidCatalog)?;
    let mut models = Vec::new();
    for provider in ["openai", "anthropic"] {
        let provider_models = root
            .get(provider)
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
            .ok_or(NativeUsageError::InvalidCatalog)?;
        for (model_id, model) in provider_models {
            let Some(cost) = model.get("cost").and_then(Value::as_object) else {
                continue;
            };
            let Some(input) = cost.get("input").and_then(decimal_nano_usd) else {
                continue;
            };
            let Some(output) = cost.get("output").and_then(decimal_nano_usd) else {
                continue;
            };
            let cache_read = cost
                .get("cache_read")
                .and_then(decimal_nano_usd)
                .unwrap_or(input);
            let cache_write = cost
                .get("cache_write")
                .and_then(decimal_nano_usd)
                .unwrap_or(input);
            let cache_read_multiplier_ppm = ratio_ppm(cache_read, input)?;
            let cache_creation_multiplier_ppm = ratio_ppm(cache_write, input)?;
            models.push(json!({
                "model": model_id,
                "inputNanoUsdPerMillion": input,
                "outputNanoUsdPerMillion": output,
                "cacheReadMultiplierPpm": cache_read_multiplier_ppm,
                "cacheCreationMultiplierPpm": cache_creation_multiplier_ppm,
            }));
        }
    }
    models.sort_by(|left, right| left["model"].as_str().cmp(&right["model"].as_str()));
    if models.is_empty()
        || models
            .windows(2)
            .any(|pair| pair[0]["model"] == pair[1]["model"])
    {
        return Err(NativeUsageError::InvalidCatalog);
    }
    let document = json!({
        "version": format!("models.dev-sha256:{}", hex_digest(bytes)),
        "source": "models.dev",
        "models": models,
    });
    PricingCatalog::from_json(&document.to_string()).map_err(|_| NativeUsageError::InvalidCatalog)
}

fn decimal_nano_usd(value: &Value) -> Option<u64> {
    let text = value.as_number()?.to_string();
    if text.contains(['e', 'E']) || text.starts_with('-') {
        return None;
    }
    let mut parts = text.split('.');
    let whole = parts.next()?.parse::<u64>().ok()?;
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some()
        || fraction.len() > 9
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let fraction_value = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().ok()? * 10_u64.pow(9 - fraction.len() as u32)
    };
    whole
        .checked_mul(1_000_000_000)?
        .checked_add(fraction_value)
        .filter(|value| *value <= i64::MAX as u64)
}

fn ratio_ppm(numerator: u64, denominator: u64) -> Result<u64, NativeUsageError> {
    if denominator == 0 {
        return Err(NativeUsageError::InvalidCatalog);
    }
    let numerator = u128::from(numerator)
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(u128::from(denominator) / 2))
        .ok_or(NativeUsageError::InvalidCatalog)?;
    let ratio = numerator / u128::from(denominator);
    u64::try_from(ratio)
        .ok()
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or(NativeUsageError::InvalidCatalog)
}

fn fingerprint(value: &str) -> String {
    hex_digest(value.as_bytes())
}

fn hex_digest(value: &[u8]) -> String {
    digest(&SHA256, value)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn target_index(target: Target) -> usize {
    match target {
        Target::Codex => 0,
        Target::Claude => 1,
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

impl RequestUsage {
    fn zero() -> Self {
        Self {
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn is_zero(self) -> bool {
        self == Self::zero()
    }

    fn positive_delta(self, prior: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(prior.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(prior.cached_input_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_sub(prior.cache_creation_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(prior.output_tokens),
        }
    }
}

impl From<tokio_rusqlite::Error<NativeUsageError>> for NativeUsageError {
    fn from(_value: tokio_rusqlite::Error<NativeUsageError>) -> Self {
        NativeUsageError::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        io::Write,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, routing::get};
    use tokio_rusqlite::rusqlite::Connection;
    use uuid::Uuid;

    use super::*;
    use crate::{
        control::protocol::{ProviderProtocol, RequestRecordOutcome, UsageActivityEntry},
        request_history::RequestRecordCompletion,
    };

    async fn fixture() -> (
        tempfile::TempDir,
        MuxviaHome,
        Arc<StateStore>,
        NativeUsageService,
    ) {
        let root = tempfile::tempdir().unwrap();
        let user_home = root.path().join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        let service = NativeUsageService::new(&home, Arc::clone(&store)).unwrap();
        (root, home, store, service)
    }

    #[test]
    fn parses_rfc3339_offsets_and_fractional_seconds() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T08:00:00.123+08:00"), Some(123));
        assert_eq!(parse_rfc3339_ms("2024-02-30T00:00:00Z"), None);
    }

    #[test]
    fn normalizes_only_first_party_models_dev_prices() {
        let catalog = normalize_models_dev(
            br#"{"openai":{"models":{"same":{"cost":{"input":1.25,"output":10,"cache_read":0.125}}}},"anthropic":{"models":{"claude":{"cost":{"input":3,"output":15,"cache_write":3.75}}}},"third":{"models":{"ignored":{"cost":{"input":1,"output":1}}}}}"#,
        )
        .unwrap();
        assert!(
            catalog
                .price(
                    "same",
                    RequestUsage {
                        input_tokens: 1_000_000,
                        cached_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        output_tokens: 0
                    },
                    1
                )
                .unwrap()
                .is_some()
        );
        assert!(
            catalog
                .price(
                    "ignored",
                    RequestUsage {
                        input_tokens: 1,
                        cached_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        output_tokens: 0
                    },
                    1
                )
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn incrementally_imports_completed_codex_lines_without_retaining_source_markers() {
        let (_root, home, _store, service) = fixture().await;
        let directory = home.user_home().join(".codex/sessions/2026/08/21");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("PROJECT_PATH_MARKER.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-08-21T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"SESSION_MARKER\"}}\n",
                "{\"timestamp\":\"2026-08-21T10:00:01Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.6\"}}\n",
                "{\"timestamp\":\"2026-08-21T10:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":12,\"cached_input_tokens\":2,\"output_tokens\":4}}}}\n",
                "{\"timestamp\":\"2026-08-21T10:00:03Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":6,\"cached_input_tokens\":1,\"output_tokens\":2}}}}",
            ),
        )
        .unwrap();

        let first = service.scan(Target::Codex).await.unwrap();
        assert_eq!(first.imported_records, 1);
        assert_eq!(
            service.scan(Target::Codex).await.unwrap().imported_records,
            0
        );
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert_eq!(
            service.scan(Target::Codex).await.unwrap().imported_records,
            1
        );

        let page = service.list(Target::Codex, None, 10).await.unwrap();
        assert_eq!(page.entries.len(), 2);
        assert!(
            page.entries
                .iter()
                .all(|entry| matches!(entry, UsageActivityEntry::NativeUsageRecord { .. }))
        );
        let database = Connection::open(home.database_path()).unwrap();
        let unsafe_text = database
            .query_row(
                "SELECT group_concat(value, '|') FROM (
                   SELECT source_record_fingerprint AS value FROM native_usage_records
                   UNION ALL SELECT model FROM native_usage_records
                   UNION ALL SELECT source_fingerprint FROM native_usage_import_cursors
                 )",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(!unsafe_text.contains("PROJECT_PATH_MARKER"));
        assert!(!unsafe_text.contains("SESSION_MARKER"));
    }

    #[tokio::test]
    async fn imports_claude_usage_then_rolls_up_and_atomically_clears_usage() {
        let (_root, home, store, service) = fixture().await;
        let directory = home.user_home().join(".claude/projects/project-marker");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("session-marker.jsonl"),
            "{\"type\":\"assistant\",\"sessionId\":\"secret-session\",\"timestamp\":\"2020-01-02T03:04:05Z\",\"message\":{\"id\":\"message-marker\",\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":1,\"output_tokens\":3}}}\n",
        )
        .unwrap();
        assert_eq!(
            service.scan(Target::Claude).await.unwrap().imported_records,
            1
        );
        let now = parse_rfc3339_ms("2026-08-21T12:00:00Z").unwrap();
        let retained = store
            .apply_usage_retention(Target::Claude, 30, now)
            .await
            .unwrap();
        assert_eq!(retained.rolled_up_days, 1);
        assert_eq!(retained.pruned_native_usage_records, 1);
        let page = service.list(Target::Claude, None, 10).await.unwrap();
        let UsageActivityEntry::DailyUsageRollup { rollup } = &page.entries[0] else {
            panic!("old native detail was not replaced by a rollup")
        };
        assert_eq!(rollup.native_usage_record_count, 1);
        assert_eq!(rollup.usage.input_tokens, 10);
        let cleared = service.clear(Target::Claude).await.unwrap();
        assert_eq!(cleared.cleared_daily_rollups, 1);
        let database = Connection::open(home.database_path()).unwrap();
        let retained_state = (
            database
                .query_row(
                    "SELECT detailed_retention_days FROM usage_settings",
                    [],
                    |row| row.get::<_, u16>(0),
                )
                .unwrap(),
            database
                .query_row("SELECT COUNT(*) FROM pricing_catalog_state", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
        );
        assert_eq!(retained_state, (30, 1));
    }

    #[tokio::test]
    async fn retention_overflow_rolls_back_setting_rollup_and_detail_changes() {
        let (_root, home, store, service) = fixture().await;
        let directory = home.user_home().join(".claude/projects/overflow-test");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("record.jsonl"),
            "{\"type\":\"assistant\",\"sessionId\":\"overflow-session\",\"timestamp\":\"2020-01-02T03:04:05Z\",\"message\":{\"id\":\"overflow-message\",\"model\":\"overflow-model\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n",
        )
        .unwrap();
        service.scan(Target::Claude).await.unwrap();
        let database = Connection::open(home.database_path()).unwrap();
        database
            .execute(
                "INSERT INTO daily_usage_rollups
                   (target, local_date, request_record_count, native_usage_record_count,
                    successful_request_count, failed_request_count, input_tokens,
                    cached_input_tokens, cache_creation_input_tokens, output_tokens,
                    priced_record_count, unpriced_record_count, estimated_cost_nano_usd,
                    latency_observation_count, total_latency_ms)
                 VALUES ('claude', '2020-01-02', 0, 0, 0, 0, ?1, 0, 0, 0, 0, 0, 0, 0, 0)",
                [i64::MAX],
            )
            .unwrap();

        let now = parse_rfc3339_ms("2026-08-21T12:00:00Z").unwrap();
        assert_eq!(
            store.apply_usage_retention(Target::Claude, 7, now).await,
            Err(NativeUsageError::Unavailable)
        );
        let retained_state = database
            .query_row(
                "SELECT
                   (SELECT detailed_retention_days FROM usage_settings),
                   (SELECT input_tokens FROM daily_usage_rollups
                    WHERE target = 'claude' AND local_date = '2020-01-02'),
                   (SELECT COUNT(*) FROM native_usage_records)",
                [],
                |row| {
                    Ok((
                        row.get::<_, u16>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(retained_state, (30, i64::MAX, 1));
    }

    #[tokio::test]
    async fn explicit_catalog_update_is_the_only_fetch_and_prices_future_completions() {
        let (_root, home, store, _service) = fixture().await;
        let native_dir = home.user_home().join(".claude/projects/catalog-test");
        fs::create_dir_all(&native_dir).unwrap();
        fs::write(
            native_dir.join("unpriced.jsonl"),
            "{\"type\":\"assistant\",\"sessionId\":\"catalog-session\",\"timestamp\":\"2026-08-21T10:00:00Z\",\"message\":{\"id\":\"catalog-message\",\"model\":\"future-model\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}}\n",
        )
        .unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let app = Router::new().route(
            "/api.json",
            get(move || {
                let request = observed.fetch_add(1, Ordering::SeqCst);
                async move {
                    if request == 0 {
                        r#"{"openai":{"models":{"future-model":{"cost":{"input":2,"output":10}}}},"anthropic":{"models":{}}}"#
                    } else {
                        r#"{"openai":{"models":{"future-model":{"cost":{"input":4,"output":20}}}},"anthropic":{"models":{}}}"#
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}/api.json", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let service =
            NativeUsageService::with_catalog_url(&home, Arc::clone(&store), origin).unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(
            service.scan(Target::Claude).await.unwrap().imported_records,
            1
        );
        let outcome = service.update_catalog(Target::Codex).await.unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(outcome.catalog_version.starts_with("models.dev-sha256:"));
        assert_eq!(outcome.backfilled_native_usage_records, 1);
        let database = Connection::open(home.database_path()).unwrap();
        let frozen_native_version = database
            .query_row(
                "SELECT catalog_version FROM native_usage_pricing_snapshots",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(frozen_native_version, outcome.catalog_version);
        let replacement = service.update_catalog(Target::Codex).await.unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_ne!(replacement.catalog_version, outcome.catalog_version);
        assert_eq!(replacement.backfilled_native_usage_records, 0);
        let still_frozen = database
            .query_row(
                "SELECT catalog_version FROM native_usage_pricing_snapshots",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(still_frozen, outcome.catalog_version);

        let completion = RequestRecordCompletion {
            id: Uuid::new_v4(),
            target: Target::Codex,
            plan_id: Uuid::new_v4(),
            plan_epoch: Uuid::new_v4(),
            provider: None,
            model: "future-model".to_owned(),
            protocol: ProviderProtocol::OpenaiResponses,
            started_at_unix_ms: 1,
            finished_at_unix_ms: 2,
            outcome: RequestRecordOutcome::Success,
            http_status: Some(200),
            usage: Some(RequestUsage {
                input_tokens: 1_000_000,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                output_tokens: 0,
            }),
            error_payload: None,
            error_payload_truncated: false,
        };
        let record_id = completion.id.to_string();
        store.record_request_completion(completion).await.unwrap();
        let priced_version = database
            .query_row(
                "SELECT catalog_version FROM pricing_snapshots WHERE request_record_id = ?1",
                [record_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(priced_version, replacement.catalog_version);
        server.abort();
    }

    #[tokio::test]
    async fn failed_clear_rolls_back_details_and_import_cursors() {
        let (_root, home, _store, service) = fixture().await;
        let directory = home.user_home().join(".claude/projects/clear-test");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("record.jsonl"),
            "{\"type\":\"assistant\",\"sessionId\":\"clear-session\",\"timestamp\":\"2026-08-21T10:00:00Z\",\"message\":{\"id\":\"clear-message\",\"model\":\"unpriced-clear-model\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n",
        )
        .unwrap();
        service.scan(Target::Claude).await.unwrap();
        let database = Connection::open(home.database_path()).unwrap();
        database
            .execute_batch(
                "CREATE TRIGGER fail_usage_clear BEFORE DELETE ON native_usage_records
                 BEGIN SELECT RAISE(ABORT, 'test-clear-failure'); END;",
            )
            .unwrap();
        assert_eq!(
            service.clear(Target::Claude).await,
            Err(NativeUsageError::Unavailable)
        );
        let counts = (
            database
                .query_row("SELECT COUNT(*) FROM native_usage_records", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            database
                .query_row(
                    "SELECT COUNT(*) FROM native_usage_import_cursors",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
        );
        assert_eq!(counts, (1, 1));
    }
}
