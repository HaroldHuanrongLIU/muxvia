use std::fmt;
use std::path::Path;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::{
    claude::{
        ClaudeCapability, ClaudeConfigCodec, ClaudeConfigSnapshot, ClaudeProblem,
        DesiredClaudeState,
    },
    codex::{
        CodexCapability, CodexConfigCodec, CodexProblem, ConfigSnapshot, DesiredCodexState,
        FileIdentity,
        config::{CodexInstalledFileState, CodexObservedDocument, CodexProviderRestoreState},
    },
    control::protocol::{
        ClaudePreflightContext, CompatibilityClassification, ProviderAuthentication,
        ProviderProtocol, ReconciliationField, ReconciliationFieldChange, ReconciliationFieldState,
        ReconciliationStrategy, ShadowSource, Target,
    },
    domain::provider::normalize_provider_base_url,
    state::RecoveryPayload,
};

pub(crate) enum TargetReconciliationAdapter {
    Codex(CodexConfigCodec),
    Claude(ClaudeConfigCodec),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProbedCompatibility {
    version: String,
    classification: CompatibilityClassification,
}

impl ProbedCompatibility {
    pub(crate) fn new(version: String, classification: CompatibilityClassification) -> Self {
        Self {
            version,
            classification,
        }
    }

    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn classification(&self) -> CompatibilityClassification {
        self.classification
    }
}

impl From<CodexCapability> for ProbedCompatibility {
    fn from(capability: CodexCapability) -> Self {
        Self::new(capability.version().to_owned(), capability.classification())
    }
}

impl From<ClaudeCapability> for ProbedCompatibility {
    fn from(capability: ClaudeCapability) -> Self {
        Self::new(capability.version().to_owned(), capability.classification())
    }
}

#[derive(Clone)]
// Keep the approved target-native value shape; this is an ephemeral coordinator input.
#[allow(clippy::large_enum_variant)]
pub(crate) enum CommittedConfiguration {
    Codex {
        desired: DesiredCodexState,
        recovery_before: ConfigSnapshot,
    },
    Claude {
        desired: DesiredClaudeState,
        recovery_before: ClaudeConfigSnapshot,
    },
}

impl fmt::Debug for CommittedConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codex { .. } => formatter.write_str("CommittedConfiguration::Codex(<redacted>)"),
            Self::Claude { .. } => {
                formatter.write_str("CommittedConfiguration::Claude(<redacted>)")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReconciliationObservation {
    pub(crate) file_identity: FileIdentity,
    pub(crate) owned_fingerprint: String,
    pub(crate) unrelated_fingerprint: String,
    pub(crate) compatibility: ProbedCompatibility,
    pub(crate) shadows: Vec<ShadowSource>,
    pub(crate) changes: Vec<ReconciliationFieldChange>,
    pub(crate) owned_drifted: bool,
}

#[derive(Clone)]
pub(crate) struct CodexRestorePreparation {
    provider: CodexProviderRestoreState,
    installed_file: CodexInstalledFileState,
}

impl fmt::Debug for CodexRestorePreparation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexRestorePreparation(<redacted>)")
    }
}

#[derive(Clone)]
// Keep the approved target-native value shape; preparation is not retained as a collection.
#[allow(clippy::large_enum_variant)]
pub(crate) enum PreparedConfiguration {
    Codex {
        before: ConfigSnapshot,
        desired: DesiredCodexState,
        recovery_before: ConfigSnapshot,
        adopted: Option<(ConfigSnapshot, DesiredCodexState)>,
        restore: Option<CodexRestorePreparation>,
        observed_document: CodexObservedDocument,
    },
    Claude {
        before: ClaudeConfigSnapshot,
        desired: DesiredClaudeState,
        recovery_before: ClaudeConfigSnapshot,
    },
}

impl fmt::Debug for PreparedConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codex { .. } => formatter.write_str("PreparedConfiguration::Codex(<redacted>)"),
            Self::Claude { .. } => formatter.write_str("PreparedConfiguration::Claude(<redacted>)"),
        }
    }
}

pub(crate) struct AdoptedProvider {
    pub(crate) target: Target,
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) base_url: String,
    pub(crate) protocol: ProviderProtocol,
    pub(crate) authentication: ProviderAuthentication,
    pub(crate) credential: SecretString,
}

impl fmt::Debug for AdoptedProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdoptedProvider")
            .field("target", &self.target)
            .field("name", &"<redacted>")
            .field("model", &"<redacted>")
            .field("base_url", &"<redacted>")
            .field("protocol", &self.protocol)
            .field("authentication", &self.authentication)
            .field("credential", &"<redacted>")
            .finish()
    }
}

impl PreparedConfiguration {
    pub(crate) fn atomic_apply(&self, user_home: &Path) -> Result<(), ReconciliationProblem> {
        match self {
            Self::Codex {
                before,
                desired,
                recovery_before,
                restore,
                ..
            } => CodexConfigCodec::for_user_home(user_home)
                .and_then(|codec| match restore {
                    Some(restore) => codec.atomic_restore_union(
                        before,
                        desired,
                        &restore.provider,
                        recovery_before,
                        &restore.installed_file,
                    ),
                    None => codec.atomic_apply(before, desired),
                })
                .map_err(map_codex_problem),
            Self::Claude {
                before, desired, ..
            } => ClaudeConfigCodec::for_user_home(user_home)
                .and_then(|codec| codec.atomic_apply(before, desired))
                .map_err(map_claude_problem),
        }
    }

    pub(crate) fn verify(&self, user_home: &Path) -> Result<(), ReconciliationProblem> {
        match self {
            Self::Codex {
                before,
                desired,
                restore,
                ..
            } => CodexConfigCodec::for_user_home(user_home)
                .and_then(|codec| match restore {
                    Some(restore) => codec.verify_restore_union(
                        before,
                        desired,
                        &restore.provider,
                        Some(&restore.installed_file),
                    ),
                    None => codec.verify(before, desired),
                })
                .map_err(map_codex_problem),
            Self::Claude {
                before, desired, ..
            } => ClaudeConfigCodec::for_user_home(user_home)
                .and_then(|codec| codec.verify(before, desired))
                .map_err(map_claude_problem),
        }
    }

    pub(crate) fn exact_rollback(&self, user_home: &Path) -> Result<(), ReconciliationProblem> {
        match self {
            Self::Codex {
                before,
                desired,
                restore,
                observed_document,
                ..
            } => CodexConfigCodec::for_user_home(user_home)
                .and_then(|codec| match restore {
                    Some(restore) => codec.exact_rollback_restore_union(
                        before,
                        desired,
                        &restore.provider,
                        Some(&restore.installed_file),
                        observed_document,
                    ),
                    None => codec.restore_or_confirm_before(before, desired),
                })
                .map_err(map_codex_problem),
            Self::Claude {
                before, desired, ..
            } => ClaudeConfigCodec::for_user_home(user_home)
                .and_then(|codec| codec.restore_or_confirm_before(before, desired))
                .map_err(map_claude_problem),
        }
    }

    pub(crate) fn durable_material(&self) -> Result<(String, String), ReconciliationProblem> {
        match self {
            Self::Codex {
                before,
                desired,
                restore,
                ..
            } => match restore {
                Some(restore) => serialize_material(
                    before,
                    &DurableCodexDesired::RestoreUnion {
                        desired: desired.clone(),
                        provider_restore: restore.provider.clone(),
                        installed_file_state: Some(restore.installed_file.clone()),
                    },
                ),
                None => serialize_material(before, desired),
            },
            Self::Claude {
                before, desired, ..
            } => serialize_material(before, desired),
        }
    }

    pub(crate) fn adopted_provider(&self) -> Result<AdoptedProvider, ReconciliationProblem> {
        match self {
            Self::Codex {
                desired, adopted, ..
            } => {
                let desired = adopted
                    .as_ref()
                    .map(|(_, desired)| desired)
                    .unwrap_or(desired);
                if desired.uses_routing_credential_header() {
                    return Err(ReconciliationProblem::new("invalid-provider-credential"));
                }
                let (name, model, base_url, credential) = desired
                    .reconciliation_provider()
                    .ok_or_else(|| ReconciliationProblem::new("invalid-configuration"))?;
                let base_url = normalize_provider_base_url(&base_url)
                    .map_err(|_| ReconciliationProblem::new("invalid-configuration"))?;
                Ok(AdoptedProvider {
                    target: Target::Codex,
                    name,
                    model,
                    base_url,
                    protocol: ProviderProtocol::OpenaiResponses,
                    authentication: ProviderAuthentication::OpenaiBearer,
                    credential,
                })
            }
            Self::Claude { desired, .. } => {
                let (model, base_url, authentication, credential) = desired
                    .reconciliation_provider()
                    .ok_or_else(|| ReconciliationProblem::new("invalid-configuration"))?;
                let base_url = normalize_provider_base_url(&base_url)
                    .map_err(|_| ReconciliationProblem::new("invalid-configuration"))?;
                Ok(AdoptedProvider {
                    target: Target::Claude,
                    name: "Adopted Claude configuration".to_owned(),
                    model,
                    base_url,
                    protocol: ProviderProtocol::AnthropicMessages,
                    authentication,
                    credential,
                })
            }
        }
    }

    pub(crate) fn adopted_recovery_payload(&self) -> RecoveryPayload {
        match self {
            Self::Codex {
                desired,
                recovery_before,
                adopted,
                ..
            } => {
                let desired = adopted
                    .as_ref()
                    .map(|(_, desired)| desired)
                    .unwrap_or(desired);
                RecoveryPayload::Codex {
                    before: Box::new(recovery_before.clone()),
                    desired: Box::new(desired.clone()),
                }
            }
            Self::Claude {
                before,
                desired,
                recovery_before,
                ..
            } => RecoveryPayload::Claude {
                before: Box::new(before.recovery_before_with_latest_unrelated(recovery_before)),
                desired: Box::new(desired.clone()),
            },
        }
    }

    pub(crate) fn managed_config_version(&self) -> u32 {
        match self {
            Self::Codex { .. } => 1,
            Self::Claude { desired, .. } => match desired.ownership() {
                crate::claude::ClaudeConfigOwnership::LegacyThree => 1,
                crate::claude::ClaudeConfigOwnership::FourField => 2,
            },
        }
    }

    pub(crate) fn file_identity_json(&self) -> Result<String, ReconciliationProblem> {
        match self {
            Self::Codex { before, .. } => serde_json::to_string(before.identity()),
            Self::Claude { before, .. } => serde_json::to_string(before.identity()),
        }
        .map_err(|_| ReconciliationProblem::new("recovery-required"))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "material", rename_all = "kebab-case")]
enum DurableCodexDesired {
    RestoreUnion {
        desired: DesiredCodexState,
        provider_restore: CodexProviderRestoreState,
        #[serde(default)]
        installed_file_state: Option<CodexInstalledFileState>,
    },
}

fn serialize_material(
    before: &impl serde::Serialize,
    desired: &impl serde::Serialize,
) -> Result<(String, String), ReconciliationProblem> {
    Ok((
        serde_json::to_string(before)
            .map_err(|_| ReconciliationProblem::new("recovery-required"))?,
        serde_json::to_string(desired)
            .map_err(|_| ReconciliationProblem::new("recovery-required"))?,
    ))
}

pub(crate) fn recover_pending_material(
    target: Target,
    user_home: &Path,
    before_json: &str,
    desired_json: &str,
) -> Result<(), ReconciliationProblem> {
    match target {
        Target::Codex => {
            let before: ConfigSnapshot = serde_json::from_str(before_json)
                .map_err(|_| ReconciliationProblem::new("recovery-required"))?;
            if serde_json::from_str::<serde_json::Value>(desired_json)
                .ok()
                .and_then(|value| value.get("material").cloned())
                .is_some()
            {
                let DurableCodexDesired::RestoreUnion {
                    desired,
                    provider_restore,
                    installed_file_state,
                } = serde_json::from_str(desired_json)
                    .map_err(|_| ReconciliationProblem::new("recovery-required"))?;
                let installed_file_state = installed_file_state
                    .ok_or_else(|| ReconciliationProblem::new("recovery-required"))?;
                return CodexConfigCodec::for_user_home(user_home)
                    .and_then(|codec| {
                        codec.restore_union_or_confirm_before(
                            &before,
                            &desired,
                            &provider_restore,
                            Some(&installed_file_state),
                        )
                    })
                    .map_err(map_codex_problem);
            }
            let desired: DesiredCodexState = serde_json::from_str(desired_json)
                .map_err(|_| ReconciliationProblem::new("recovery-required"))?;
            CodexConfigCodec::for_user_home(user_home)
                .and_then(|codec| codec.restore_or_confirm_before(&before, &desired))
                .map_err(map_codex_problem)
        }
        Target::Claude => {
            let before: ClaudeConfigSnapshot = serde_json::from_str(before_json)
                .map_err(|_| ReconciliationProblem::new("recovery-required"))?;
            let desired: DesiredClaudeState = serde_json::from_str(desired_json)
                .map_err(|_| ReconciliationProblem::new("recovery-required"))?;
            ClaudeConfigCodec::for_user_home(user_home)
                .and_then(|codec| codec.restore_or_confirm_before(&before, &desired))
                .map_err(map_claude_problem)
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ObservedReconciliation {
    pub(crate) observation: ReconciliationObservation,
    pub(crate) prepared: PreparedConfiguration,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ReconciliationContext {
    Codex,
    Claude(ClaudePreflightContext),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReconciliationProblem {
    code: &'static str,
}

impl ReconciliationProblem {
    fn new(code: &'static str) -> Self {
        Self { code }
    }

    pub(crate) fn fixed(code: &'static str) -> Self {
        Self::new(code)
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Debug for ReconciliationProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReconciliationProblem")
            .field("code", &self.code)
            .finish()
    }
}

impl fmt::Display for ReconciliationProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for ReconciliationProblem {}

impl TargetReconciliationAdapter {
    pub(crate) fn observe(
        &self,
        strategy: ReconciliationStrategy,
        committed: &CommittedConfiguration,
        context: &ReconciliationContext,
        compatibility: ProbedCompatibility,
    ) -> Result<ObservedReconciliation, ReconciliationProblem> {
        match (self, committed, context) {
            (
                Self::Codex(codec),
                CommittedConfiguration::Codex {
                    desired,
                    recovery_before,
                },
                ReconciliationContext::Codex,
            ) => observe_codex(codec, strategy, desired, recovery_before, compatibility),
            (
                Self::Claude(codec),
                CommittedConfiguration::Claude {
                    desired,
                    recovery_before,
                },
                ReconciliationContext::Claude(context),
            ) => observe_claude(
                codec,
                strategy,
                desired,
                recovery_before,
                context,
                compatibility,
            ),
            _ => Err(ReconciliationProblem::new("recovery-required")),
        }
    }
}

fn observe_codex(
    codec: &CodexConfigCodec,
    strategy: ReconciliationStrategy,
    committed: &DesiredCodexState,
    recovery_before: &ConfigSnapshot,
    compatibility: ProbedCompatibility,
) -> Result<ObservedReconciliation, ReconciliationProblem> {
    let (current, selected, observed_document, profile_shadow) = codec
        .reconciliation_snapshots_for(committed)
        .map_err(map_codex_problem)?;
    let compared = if strategy == ReconciliationStrategy::Adopt {
        &selected
    } else {
        &current
    };
    let provider_changed = !compared.provider_matches(committed);
    let credential_changed = !compared.credential_matches(committed);
    let before = current.clone();
    let adopted = (strategy == ReconciliationStrategy::Adopt)
        .then(|| (selected.clone(), selected.as_adopted_direct()));
    let recovery_before = match (strategy, adopted.as_ref()) {
        (ReconciliationStrategy::Adopt, Some((_, adopted_desired))) => recovery_before
            .with_provider_restore_for(adopted_desired)
            .map_err(map_codex_problem)?,
        (ReconciliationStrategy::Restore, _) => recovery_before
            .with_provider_restore_for(committed)
            .map_err(map_codex_problem)?,
        _ => recovery_before.clone(),
    };
    let desired = match strategy {
        ReconciliationStrategy::Adopt => current.as_desired_like(committed),
        ReconciliationStrategy::Reapply => committed.clone(),
        ReconciliationStrategy::Restore => recovery_before.as_desired_like(committed),
    };
    let restore_provider = (strategy == ReconciliationStrategy::Restore)
        .then(|| recovery_before.provider_restore().cloned())
        .flatten();
    let restore = restore_provider
        .map(|provider| {
            observed_document
                .planned_restore_union_file_state(&before, &desired, &provider, &recovery_before)
                .map(|installed_file| CodexRestorePreparation {
                    provider,
                    installed_file,
                })
        })
        .transpose()
        .map_err(map_codex_problem)?;
    Ok(ObservedReconciliation {
        observation: ReconciliationObservation {
            file_identity: current.identity().clone(),
            owned_fingerprint: current.owned_fingerprint(),
            unrelated_fingerprint: current.unrelated_fingerprint(),
            compatibility,
            shadows: profile_shadow
                .then_some(ShadowSource::CodexProfile)
                .into_iter()
                .collect(),
            changes: changes(strategy, provider_changed, credential_changed),
            owned_drifted: provider_changed || credential_changed,
        },
        prepared: PreparedConfiguration::Codex {
            before,
            desired,
            recovery_before,
            adopted,
            restore,
            observed_document,
        },
    })
}

fn observe_claude(
    codec: &ClaudeConfigCodec,
    strategy: ReconciliationStrategy,
    committed: &DesiredClaudeState,
    recovery_before: &ClaudeConfigSnapshot,
    context: &ClaudePreflightContext,
    compatibility: ProbedCompatibility,
) -> Result<ObservedReconciliation, ReconciliationProblem> {
    if committed.ownership() != recovery_before.ownership() {
        return Err(ReconciliationProblem::new("recovery-required"));
    }
    let (current, shadows) = codec
        .reconciliation_snapshot(context, committed.ownership())
        .map_err(map_claude_problem)?;
    let provider_changed = !current.provider_matches(committed);
    let credential_changed = !current.credential_matches(committed);
    let before = current.clone();
    let desired = match strategy {
        ReconciliationStrategy::Adopt => current.as_adopted_direct(),
        ReconciliationStrategy::Reapply => committed.clone(),
        ReconciliationStrategy::Restore => recovery_before.as_desired_like(committed),
    };
    Ok(ObservedReconciliation {
        observation: ReconciliationObservation {
            file_identity: current.identity().clone(),
            owned_fingerprint: current.owned_fingerprint(),
            unrelated_fingerprint: current.unrelated_fingerprint().to_owned(),
            compatibility,
            shadows,
            changes: changes(strategy, provider_changed, credential_changed),
            owned_drifted: provider_changed || credential_changed,
        },
        prepared: PreparedConfiguration::Claude {
            before,
            desired,
            recovery_before: recovery_before.clone(),
        },
    })
}

fn changes(
    strategy: ReconciliationStrategy,
    provider_changed: bool,
    credential_changed: bool,
) -> Vec<ReconciliationFieldChange> {
    let unchanged = ReconciliationFieldState::Unchanged;
    let changed = ReconciliationFieldState::Changed;
    match strategy {
        ReconciliationStrategy::Adopt => vec![
            change(
                ReconciliationField::Provider,
                if provider_changed { changed } else { unchanged },
            ),
            change(
                ReconciliationField::Credential,
                if credential_changed {
                    changed
                } else {
                    unchanged
                },
            ),
            change(ReconciliationField::CurrentProvider, changed),
            change(ReconciliationField::ActivatedSnapshot, changed),
            change(ReconciliationField::Takeover, unchanged),
        ],
        ReconciliationStrategy::Reapply => vec![
            change(ReconciliationField::Provider, unchanged),
            change(ReconciliationField::Credential, unchanged),
            change(ReconciliationField::CurrentProvider, unchanged),
            change(ReconciliationField::ActivatedSnapshot, unchanged),
            change(ReconciliationField::Takeover, unchanged),
        ],
        ReconciliationStrategy::Restore => vec![
            change(ReconciliationField::Provider, unchanged),
            change(ReconciliationField::Credential, unchanged),
            change(
                ReconciliationField::CurrentProvider,
                ReconciliationFieldState::Absent,
            ),
            change(
                ReconciliationField::ActivatedSnapshot,
                ReconciliationFieldState::Absent,
            ),
            change(
                ReconciliationField::Takeover,
                ReconciliationFieldState::Absent,
            ),
        ],
    }
}

fn change(
    field: ReconciliationField,
    state: ReconciliationFieldState,
) -> ReconciliationFieldChange {
    ReconciliationFieldChange { field, state }
}

fn map_codex_problem(problem: CodexProblem) -> ReconciliationProblem {
    ReconciliationProblem::new(match problem.code() {
        "configuration-collision" => "configuration-drift",
        code => code,
    })
}

fn map_claude_problem(problem: ClaudeProblem) -> ReconciliationProblem {
    ReconciliationProblem::new(match problem.code() {
        "configuration-collision" => "configuration-drift",
        code => code,
    })
}

#[cfg(test)]
mod tests {
    use std::{fmt, fs};

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    use secrecy::ExposeSecret;
    use tempfile::TempDir;

    use super::{
        CommittedConfiguration, PreparedConfiguration, ProbedCompatibility, ReconciliationContext,
        TargetReconciliationAdapter,
    };
    use crate::{
        claude::{ClaudeConfigCodec, ClaudeConfigOwnership},
        codex::{CodexConfigCodec, config::ManagedCodexMode},
        control::protocol::{
            ClaudeBlockingSelector, ClaudeHostManagedState, ClaudePreflightContext,
            ClaudeSelectorState, CompatibilityClassification, ReconciliationField,
            ReconciliationFieldChange, ReconciliationFieldState, ReconciliationStrategy,
            ShadowSource,
        },
    };

    fn compatibility() -> ProbedCompatibility {
        ProbedCompatibility::new(
            "codex-cli 0.106.0".to_owned(),
            CompatibilityClassification::Tested,
        )
    }

    fn claude_compatibility() -> ProbedCompatibility {
        ProbedCompatibility::new(
            "2.1.37 (Claude Code)".to_owned(),
            CompatibilityClassification::Tested,
        )
    }

    fn claude_context(cwd: &std::path::Path) -> ClaudePreflightContext {
        ClaudePreflightContext {
            claude_config_dir: None,
            selector_state: ClaudeSelectorState::Unset,
            blocking_selector: None,
            host_managed_state: ClaudeHostManagedState::Unmanaged,
            cwd: cwd.to_string_lossy().into_owned(),
        }
    }

    #[cfg(unix)]
    #[derive(PartialEq, Eq)]
    struct ShadowFingerprint {
        bytes: Vec<u8>,
        mode: u32,
        modified: std::time::SystemTime,
    }

    #[cfg(unix)]
    impl fmt::Debug for ShadowFingerprint {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("ShadowFingerprint")
                .field("bytes", &"<redacted>")
                .field("mode", &self.mode)
                .field("modified", &self.modified)
                .finish()
        }
    }

    #[cfg(unix)]
    fn shadow_fingerprint(path: &std::path::Path) -> ShadowFingerprint {
        let metadata = fs::metadata(path).unwrap();
        ShadowFingerprint {
            bytes: fs::read(path).unwrap(),
            mode: metadata.permissions().mode() & 0o777,
            modified: metadata.modified().unwrap(),
        }
    }

    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_owned())
    }

    #[test]
    fn reconciliation_codex_strategies_prepare_only_redacted_effects() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(codec.config_path(), "approval_policy = \"never\"\n").unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_takeover(
            "committed-model-sentinel",
            "http://127.0.0.1:43123/v1/committed-url-sentinel",
            "committed-credential-sentinel",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        fs::write(
            codec.config_path(),
            r#"model = "observed-model-sentinel"
model_provider = "muxvia_codex"
operator_setting = "unrelated-sentinel"

[model_providers.muxvia_codex]
name = "Muxvia"
base_url = "https://observed-url-sentinel.example/v1"
wire_api = "responses"
http_headers = { "X-Muxvia-Routing-Credential" = "observed-credential-sentinel" }
supports_websockets = false
"#,
        )
        .unwrap();
        let observed = codec.desired_takeover(
            "observed-model-sentinel",
            "https://observed-url-sentinel.example/v1",
            "observed-credential-sentinel",
        );
        let recovery_desired: crate::codex::DesiredCodexState =
            serde_json::from_value(serde_json::json!({
                "model": null,
                "model_provider": null,
                "provider_name": null,
                "provider_base_url": null,
                "provider_wire_api": null,
                "provider_http_headers": null,
                "provider_supports_websockets": null
            }))
            .unwrap();

        for (strategy, expected) in [
            (
                ReconciliationStrategy::Adopt,
                vec![
                    ReconciliationFieldChange {
                        field: ReconciliationField::Provider,
                        state: ReconciliationFieldState::Changed,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::Credential,
                        state: ReconciliationFieldState::Changed,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::CurrentProvider,
                        state: ReconciliationFieldState::Changed,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::ActivatedSnapshot,
                        state: ReconciliationFieldState::Changed,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::Takeover,
                        state: ReconciliationFieldState::Unchanged,
                    },
                ],
            ),
            (
                ReconciliationStrategy::Reapply,
                vec![
                    ReconciliationFieldChange {
                        field: ReconciliationField::Provider,
                        state: ReconciliationFieldState::Unchanged,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::Credential,
                        state: ReconciliationFieldState::Unchanged,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::CurrentProvider,
                        state: ReconciliationFieldState::Unchanged,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::ActivatedSnapshot,
                        state: ReconciliationFieldState::Unchanged,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::Takeover,
                        state: ReconciliationFieldState::Unchanged,
                    },
                ],
            ),
            (
                ReconciliationStrategy::Restore,
                vec![
                    ReconciliationFieldChange {
                        field: ReconciliationField::Provider,
                        state: ReconciliationFieldState::Unchanged,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::Credential,
                        state: ReconciliationFieldState::Unchanged,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::CurrentProvider,
                        state: ReconciliationFieldState::Absent,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::ActivatedSnapshot,
                        state: ReconciliationFieldState::Absent,
                    },
                    ReconciliationFieldChange {
                        field: ReconciliationField::Takeover,
                        state: ReconciliationFieldState::Absent,
                    },
                ],
            ),
        ] {
            let adapter = TargetReconciliationAdapter::Codex(
                CodexConfigCodec::for_user_home(home.path()).unwrap(),
            );
            let result = adapter
                .observe(
                    strategy,
                    &CommittedConfiguration::Codex {
                        desired: committed.clone(),
                        recovery_before: recovery_before.clone(),
                    },
                    &ReconciliationContext::Codex,
                    compatibility(),
                )
                .unwrap();
            assert_eq!(result.observation.changes, expected);
            assert_eq!(result.observation.shadows, Vec::<ShadowSource>::new());
            match &result.prepared {
                PreparedConfiguration::Codex {
                    before, desired, ..
                } => {
                    assert_eq!(before.as_desired_like(&committed), observed);
                    match strategy {
                        ReconciliationStrategy::Adopt => {
                            assert_eq!(desired, &observed);
                            assert_eq!(desired.mode(), Some(ManagedCodexMode::Takeover));
                        }
                        ReconciliationStrategy::Reapply => {
                            assert_eq!(desired, &committed);
                            assert_eq!(desired.mode(), Some(ManagedCodexMode::Takeover));
                        }
                        ReconciliationStrategy::Restore => {
                            assert_eq!(desired, &recovery_desired);
                            assert_eq!(desired.mode(), Some(ManagedCodexMode::Takeover));
                        }
                    }
                }
                PreparedConfiguration::Claude { .. } => panic!("wrong prepared target"),
            }
            let serialized = serde_json::to_string(&result.observation.changes).unwrap();
            let panic_text = panic_message(
                std::panic::catch_unwind(|| panic!("{result:?}"))
                    .expect_err("diagnostic panic mutation did not panic"),
            );
            let diagnostic = format!("{result:?}\n{serialized}\n{panic_text}");
            for sentinel in [
                "committed-model-sentinel",
                "committed-url-sentinel",
                "committed-credential-sentinel",
                "observed-model-sentinel",
                "observed-url-sentinel",
                "observed-credential-sentinel",
                "unrelated-sentinel",
            ] {
                assert!(!diagnostic.contains(sentinel));
                assert!(!diagnostic.contains(&format!("{:?}", sentinel.as_bytes())));
            }
            assert_eq!(result.observation.owned_fingerprint.len(), 64);
            assert_eq!(result.observation.unrelated_fingerprint.len(), 64);
        }
    }

    #[test]
    fn reconciliation_prepared_configuration_exactly_rolls_back_observed_snapshot() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(codec.config_path(), "operator = \"before\"\n").unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_takeover(
            "ROLLBACK_COMMITTED_MODEL_96601",
            "http://127.0.0.1:43123/v1",
            "ROLLBACK_COMMITTED_SECRET_96602",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        fs::write(
            codec.config_path(),
            r#"operator = "latest-unrelated"
model = "ROLLBACK_OBSERVED_MODEL_96603"
model_provider = "muxvia_codex"

[model_providers.muxvia_codex]
name = "Muxvia"
base_url = "https://rollback-observed.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer ROLLBACK_OBSERVED_SECRET_96604" }
supports_websockets = false
"#,
        )
        .unwrap();
        let observed_bytes = fs::read(codec.config_path()).unwrap();
        #[cfg(unix)]
        let observed_mode = fs::metadata(codec.config_path())
            .unwrap()
            .permissions()
            .mode();
        let prepared = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Reapply,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap()
            .prepared;
        let diagnostic = format!("{prepared:?}");
        for sentinel in [
            "ROLLBACK_COMMITTED_MODEL_96601",
            "ROLLBACK_COMMITTED_SECRET_96602",
            "ROLLBACK_OBSERVED_MODEL_96603",
            "ROLLBACK_OBSERVED_SECRET_96604",
        ] {
            assert!(!diagnostic.contains(sentinel));
        }
        prepared.atomic_apply(home.path()).unwrap();
        prepared.verify(home.path()).unwrap();
        prepared.exact_rollback(home.path()).unwrap();
        let config_path = home.path().join(".codex/config.toml");
        assert!(
            fs::read(&config_path).unwrap() == observed_bytes,
            "rollback did not restore the exact observed bytes"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(config_path).unwrap().permissions().mode(),
            observed_mode
        );
    }

    #[test]
    fn reconciliation_codex_profile_is_a_closed_read_only_shadow() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(codec.config_path(), "approval_policy = \"never\"\n").unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_takeover("m", "http://127.0.0.1:9/v1", "c");
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        let mut bytes = b"profile = \"operator-profile-sentinel\"\n".to_vec();
        bytes.extend_from_slice(&fs::read(codec.config_path()).unwrap());
        fs::write(codec.config_path(), &bytes).unwrap();
        let before = fs::read(codec.config_path()).unwrap();

        let result = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Reapply,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();

        assert_eq!(result.observation.shadows, vec![ShadowSource::CodexProfile]);
        assert_eq!(
            fs::read(home.path().join(".codex/config.toml")).unwrap(),
            before
        );
    }

    #[test]
    fn codex_adopt_uses_and_validates_the_selected_provider_table() {
        let valid = r#"model = "selected-model"
model_provider = "operator_openai"

[model_providers.muxvia_codex]
name = "Stale Muxvia"
base_url = "https://stale.invalid/v1"
wire_api = "chat"
http_headers = { Authorization = "Bearer STALE_SECRET_97101" }
supports_websockets = true

[model_providers.operator_openai]
name = "Operator OpenAI"
base_url = "https://selected.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer SELECTED_SECRET_97102" }
supports_websockets = false
"#;
        let invalid = [
            valid.replace(
                "operator_openai\"\n\n[model_providers.muxvia_codex]",
                "missing\"\n\n[model_providers.muxvia_codex]",
            ),
            valid.replace("wire_api = \"responses\"", "wire_api = \"chat\""),
            valid.replace("supports_websockets = false", "supports_websockets = true"),
            valid.replace("name = \"Operator OpenAI\"\n", ""),
            valid.replace("name = \"Operator OpenAI\"", "name = \"   \""),
            valid.replace("model = \"selected-model\"", "model = \"   \""),
            valid.replace(
                "Authorization = \"Bearer SELECTED_SECRET_97102\"",
                "Authorization = \"Bearer    \"",
            ),
            valid.replace(
                "Authorization = \"Bearer SELECTED_SECRET_97102\"",
                "\"X-Muxvia-Routing-Credential\" = \"DIFFERENT_ROUTE_SECRET_97302\"",
            ),
            valid.replace(
                "Authorization = \"Bearer SELECTED_SECRET_97102\"",
                "Authorization = \"Bearer SELECTED_SECRET_97102\", \"X-Muxvia-Routing-Credential\" = \"DIFFERENT_ROUTE_SECRET_97302\"",
            ),
            valid.replace("model = \"selected-model\"", "model = 7"),
            valid.replace(
                "base_url = \"https://selected.example/v1\"",
                "base_url = false",
            ),
            valid.replace(
                "https://selected.example/v1",
                "http://non-loopback.example/v1",
            ),
        ];

        for (configuration, accepted) in std::iter::once((valid.to_owned(), true)).chain(
            invalid
                .into_iter()
                .map(|configuration| (configuration, false)),
        ) {
            let home = TempDir::new().unwrap();
            let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
            fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
            let recovery_before = codec.inspect().unwrap();
            fs::write(codec.config_path(), &configuration).unwrap();
            let committed = codec.desired_direct(
                "committed-model",
                "https://committed.example/v1",
                "COMMITTED_SECRET_97103",
            );
            let observed = TargetReconciliationAdapter::Codex(codec)
                .observe(
                    ReconciliationStrategy::Adopt,
                    &CommittedConfiguration::Codex {
                        desired: committed,
                        recovery_before,
                    },
                    &ReconciliationContext::Codex,
                    compatibility(),
                )
                .unwrap();
            let adopted = observed.prepared.adopted_provider();
            if accepted {
                let adopted = adopted.unwrap();
                assert_eq!(adopted.model, "selected-model");
                assert_eq!(adopted.base_url, "https://selected.example/v1");
                assert_eq!(adopted.credential.expose_secret(), "SELECTED_SECRET_97102");
                assert_eq!(
                    observed.observation.changes[1].state,
                    ReconciliationFieldState::Changed
                );
                let diagnostic = format!("{observed:?}\n{adopted:?}");
                for secret in ["STALE_SECRET_97101", "SELECTED_SECRET_97102"] {
                    assert!(!diagnostic.contains(secret));
                    assert!(!diagnostic.contains(&format!("{:?}", secret.as_bytes())));
                }
            } else {
                let expected = if configuration.contains("DIFFERENT_ROUTE_SECRET_97302") {
                    "invalid-provider-credential"
                } else {
                    "invalid-configuration"
                };
                assert_eq!(adopted.unwrap_err().code(), expected);
            }
        }
    }

    #[test]
    fn codex_adopt_rejects_an_unrepresentable_bound_provider_before_without_mutation() {
        let historical_variants = [
            (
                "wire_api = \"chat\"",
                "http_headers = { Authorization = \"Bearer HISTORICAL_SECRET_97401\" }",
                "supports_websockets = false",
            ),
            (
                "wire_api = \"responses\"",
                "http_headers = { Authorization = \"Bearer HISTORICAL_SECRET_97401\" }",
                "supports_websockets = true",
            ),
            (
                "wire_api = \"responses\"",
                "http_headers = { Authorization = \"Bearer HISTORICAL_SECRET_97401\", \"X-Operator-Secret\" = \"UNRELATED_SECRET_97402\" }",
                "supports_websockets = false",
            ),
        ];

        for (wire_api, headers, websocket) in historical_variants {
            let home = TempDir::new().unwrap();
            let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
            fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
            let historical = format!(
                r#"model = "historical-model"
model_provider = "operator"
unrelated_secret = "UNRELATED_SECRET_97403"

[model_providers.operator]
name = "Historical operator"
base_url = "https://historical.example/v1"
{wire_api}
{headers}
{websocket}
operator_decoration = "preserve"
"#
            );
            fs::write(codec.config_path(), &historical).unwrap();
            let recovery_before = codec.inspect().unwrap();
            let committed = codec.desired_direct(
                "committed-model",
                "https://committed.example/v1",
                "COMMITTED_SECRET_97404",
            );
            codec.atomic_apply(&recovery_before, &committed).unwrap();
            let external = r#"model = "external-model"
model_provider = "operator"
unrelated_secret = "UNRELATED_SECRET_97403"

[model_providers.operator]
name = "External operator"
base_url = "https://external.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer EXTERNAL_SECRET_97405" }
supports_websockets = false
operator_decoration = "preserve"
"#;
            fs::write(codec.config_path(), external).unwrap();
            let before = fs::read(codec.config_path()).unwrap();
            let before_mode = fs::metadata(codec.config_path())
                .unwrap()
                .permissions()
                .mode();

            let error = TargetReconciliationAdapter::Codex(codec)
                .observe(
                    ReconciliationStrategy::Adopt,
                    &CommittedConfiguration::Codex {
                        desired: committed,
                        recovery_before,
                    },
                    &ReconciliationContext::Codex,
                    compatibility(),
                )
                .unwrap_err();

            assert_eq!(error.code(), "invalid-configuration");
            let diagnostic = format!("{error:?}");
            for secret in [
                "HISTORICAL_SECRET_97401",
                "UNRELATED_SECRET_97402",
                "UNRELATED_SECRET_97403",
                "COMMITTED_SECRET_97404",
                "EXTERNAL_SECRET_97405",
            ] {
                assert!(!diagnostic.contains(secret), "diagnostic leaked a secret");
                assert!(
                    !diagnostic.contains(&format!("{:?}", secret.as_bytes())),
                    "diagnostic leaked numeric secret bytes"
                );
            }
            assert!(
                fs::read(home.path().join(".codex/config.toml")).unwrap() == before,
                "rejected Adopt changed the Managed Configuration"
            );
            assert_eq!(
                fs::metadata(home.path().join(".codex/config.toml"))
                    .unwrap()
                    .permissions()
                    .mode(),
                before_mode
            );
        }
    }

    #[test]
    fn codex_reapply_preserves_an_externally_selected_provider_table_and_rolls_back() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_direct(
            "committed-model",
            "https://committed.example/v1",
            "COMMITTED_SECRET_97201",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        let managed = fs::read_to_string(codec.config_path()).unwrap();
        let drifted = managed
            .replace("model = \"committed-model\"", "model = \"external-model\"")
            .replace(
                "model_provider = \"muxvia_codex\"",
                "model_provider = \"external\"",
            )
            + r#"
[model_providers.external]
name = "External"
base_url = "https://external.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer EXTERNAL_SECRET_97202" }
supports_websockets = false
operator_note = "preserve-me"
"#;
        fs::write(codec.config_path(), &drifted).unwrap();

        let observed = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Reapply,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();
        observed.prepared.atomic_apply(home.path()).unwrap();
        observed.prepared.verify(home.path()).unwrap();
        let reapplied = fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
        assert!(reapplied.contains("model_provider = \"muxvia_codex\""));
        assert!(reapplied.contains("[model_providers.external]"));
        assert!(reapplied.contains("operator_note = \"preserve-me\""));

        observed.prepared.exact_rollback(home.path()).unwrap();
        let rolled_back = fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
        assert!(rolled_back.contains("model_provider = \"external\""));
        assert!(rolled_back.contains("EXTERNAL_SECRET_97202"));
        assert!(rolled_back.contains("operator_note = \"preserve-me\""));
    }

    #[test]
    fn codex_reapply_keeps_bound_provider_ownership_when_the_selector_is_absent_or_nonstring() {
        for selector in [None, Some("model_provider = 7 # operator mutation\n")] {
            let home = TempDir::new().unwrap();
            let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
            let recovery_before = codec.inspect().unwrap();
            let committed = codec.desired_direct(
                "committed-model",
                "https://committed.example/v1",
                "COMMITTED_SECRET_97501",
            );
            codec.atomic_apply(&recovery_before, &committed).unwrap();
            let managed = fs::read_to_string(codec.config_path()).unwrap();
            let drifted = managed
                .replace("model = \"committed-model\"", "model = \"drifted-model\"")
                .replace(
                    "model_provider = \"muxvia_codex\"\n",
                    selector.unwrap_or(""),
                );
            fs::write(codec.config_path(), &drifted).unwrap();
            let observed_bytes = fs::read(codec.config_path()).unwrap();

            let observed = TargetReconciliationAdapter::Codex(codec)
                .observe(
                    ReconciliationStrategy::Reapply,
                    &CommittedConfiguration::Codex {
                        desired: committed.clone(),
                        recovery_before: recovery_before.clone(),
                    },
                    &ReconciliationContext::Codex,
                    compatibility(),
                )
                .unwrap();
            let (pending_before, pending_desired) = observed.prepared.durable_material().unwrap();
            observed.prepared.atomic_apply(home.path()).unwrap();
            observed.prepared.verify(home.path()).unwrap();
            let reapplied = fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
            assert!(reapplied.contains("model_provider = \"muxvia_codex\""));
            assert!(reapplied.contains("[model_providers.muxvia_codex]"));

            observed.prepared.exact_rollback(home.path()).unwrap();
            assert!(
                fs::read(home.path().join(".codex/config.toml")).unwrap() == observed_bytes,
                "exact rollback changed the selector-mutated Managed Configuration"
            );

            let observed = TargetReconciliationAdapter::Codex(
                CodexConfigCodec::for_user_home(home.path()).unwrap(),
            )
            .observe(
                ReconciliationStrategy::Reapply,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();
            observed.prepared.atomic_apply(home.path()).unwrap();
            super::recover_pending_material(
                crate::control::protocol::Target::Codex,
                home.path(),
                &pending_before,
                &pending_desired,
            )
            .unwrap();
            assert!(
                fs::read(home.path().join(".codex/config.toml")).unwrap() == observed_bytes,
                "startup recovery changed the selector-mutated Managed Configuration"
            );
        }
    }

    #[test]
    fn codex_adopt_then_restore_uses_bound_missing_snapshot_ownership() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_direct(
            "committed-model",
            "https://committed.example/v1",
            "COMMITTED_SECRET_97203",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        let managed = fs::read_to_string(codec.config_path()).unwrap();
        let external = managed
            .replace("model = \"committed-model\"", "model = \"external-model\"")
            .replace(
                "model_provider = \"muxvia_codex\"",
                "model_provider = \"external\"",
            )
            + r#"
[model_providers.external]
name = "External"
base_url = "https://external.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer EXTERNAL_SECRET_97204" }
supports_websockets = false
"#;
        fs::write(codec.config_path(), external).unwrap();

        let adopted = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Adopt,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();
        let crate::state::RecoveryPayload::Codex { before, desired } =
            adopted.prepared.adopted_recovery_payload()
        else {
            panic!("Codex Adopt must retain Codex recovery material")
        };
        let restored = TargetReconciliationAdapter::Codex(
            CodexConfigCodec::for_user_home(home.path()).unwrap(),
        )
        .observe(
            ReconciliationStrategy::Restore,
            &CommittedConfiguration::Codex {
                desired: (*desired).clone(),
                recovery_before: (*before).clone(),
            },
            &ReconciliationContext::Codex,
            compatibility(),
        )
        .unwrap();
        restored.prepared.atomic_apply(home.path()).unwrap();
        restored.prepared.verify(home.path()).unwrap();
        assert!(
            !home.path().join(".codex/config.toml").exists(),
            "Restore retained provider partitions absent from the bound historical snapshot"
        );

        restored.prepared.exact_rollback(home.path()).unwrap();
        let rolled_back = fs::read_to_string(home.path().join(".codex/config.toml")).unwrap();
        assert!(rolled_back.contains("model_provider = \"external\""));
        assert!(rolled_back.contains("EXTERNAL_SECRET_97204"));
        assert!(rolled_back.contains("[model_providers.muxvia_codex]"));
    }

    #[test]
    fn codex_adopt_restore_removes_a_historically_absent_file_without_unrelated_values() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_direct(
            "committed-model",
            "https://committed.example/v1",
            "COMMITTED_SECRET_97406",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        let external = r#"model = "external-model"
model_provider = "external"

[model_providers.external]
name = "External"
base_url = "https://external.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer EXTERNAL_SECRET_97407" }
supports_websockets = false
"#;
        fs::write(codec.config_path(), external).unwrap();

        let adopted = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Adopt,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();
        let crate::state::RecoveryPayload::Codex { before, desired } =
            adopted.prepared.adopted_recovery_payload()
        else {
            panic!("Codex Adopt must retain Codex recovery material")
        };
        let restored = TargetReconciliationAdapter::Codex(
            CodexConfigCodec::for_user_home(home.path()).unwrap(),
        )
        .observe(
            ReconciliationStrategy::Restore,
            &CommittedConfiguration::Codex {
                desired: (*desired).clone(),
                recovery_before: (*before).clone(),
            },
            &ReconciliationContext::Codex,
            compatibility(),
        )
        .unwrap();

        restored.prepared.atomic_apply(home.path()).unwrap();
        restored.prepared.verify(home.path()).unwrap();
        assert!(
            !home.path().join(".codex/config.toml").exists(),
            "Restore retained a historically absent empty Managed Configuration"
        );

        restored.prepared.exact_rollback(home.path()).unwrap();
        assert!(
            fs::read(home.path().join(".codex/config.toml")).unwrap() == external.as_bytes(),
            "exact rollback did not restore the observed Managed Configuration"
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_adopt_restore_preserves_the_exact_bound_same_key_historical_document() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let config_path = codec.config_path().to_owned();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        let historical = r#"# historical top decoration
model = "historical-model" # historical model decoration
model_provider = "operator" # historical selector decoration
operator_setting = { keep = "historical-unrelated" }

[model_providers.operator]
name = "Historical Operator" # historical provider decoration
base_url = "https://historical.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer HISTORICAL_SECRET_97305" }
supports_websockets = false
operator_note = "same-key-unrelated"
"#;
        fs::write(codec.config_path(), historical).unwrap();
        fs::set_permissions(codec.config_path(), fs::Permissions::from_mode(0o640)).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_direct(
            "committed-model",
            "https://committed.example/v1",
            "COMMITTED_SECRET_97306",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        let managed = fs::read_to_string(codec.config_path()).unwrap();
        let external = managed
            .replace("model = \"committed-model\"", "model = \"adopted-model\"")
            .replace(
                "model_provider = \"muxvia_codex\"",
                "model_provider = \"operator\"",
            )
            .replace("Historical Operator", "Adopted Operator")
            .replace(
                "https://historical.example/v1",
                "https://adopted.example/v1",
            )
            .replace("HISTORICAL_SECRET_97305", "ADOPTED_SECRET_97307")
            + r#"

[current_only]
unrelated_secret = "CURRENT_ONLY_UNRELATED_SECRET_97309"
"#;
        fs::write(codec.config_path(), &external).unwrap();
        fs::set_permissions(codec.config_path(), fs::Permissions::from_mode(0o600)).unwrap();

        let adopted = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Adopt,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();
        let recovery_payload = adopted.prepared.adopted_recovery_payload();
        let serialized_recovery = serde_json::to_string(&recovery_payload).unwrap();
        let current_only_secret_absent =
            !serialized_recovery.contains("CURRENT_ONLY_UNRELATED_SECRET_97309");
        assert!(
            current_only_secret_absent,
            "recovery payload included a current-only unrelated value"
        );
        let recovery_diagnostic = format!("{recovery_payload:?}");
        for secret in [
            "HISTORICAL_SECRET_97305",
            "ADOPTED_SECRET_97307",
            "CURRENT_ONLY_UNRELATED_SECRET_97309",
        ] {
            assert!(!recovery_diagnostic.contains(secret));
            assert!(!recovery_diagnostic.contains(&format!("{:?}", secret.as_bytes())));
        }
        let crate::state::RecoveryPayload::Codex { before, desired } = recovery_payload else {
            panic!("Codex Adopt must retain Codex recovery material")
        };
        let committed_after_adopt = CommittedConfiguration::Codex {
            desired: (*desired).clone(),
            recovery_before: (*before).clone(),
        };
        let restored = TargetReconciliationAdapter::Codex(
            CodexConfigCodec::for_user_home(home.path()).unwrap(),
        )
        .observe(
            ReconciliationStrategy::Restore,
            &committed_after_adopt,
            &ReconciliationContext::Codex,
            compatibility(),
        )
        .unwrap();
        restored.prepared.atomic_apply(home.path()).unwrap();
        restored.prepared.verify(home.path()).unwrap();
        restored.prepared.exact_rollback(home.path()).unwrap();
        assert!(
            fs::read(&config_path).unwrap() == external.as_bytes(),
            "exact rollback changed the observed Codex document"
        );
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let restored = TargetReconciliationAdapter::Codex(
            CodexConfigCodec::for_user_home(home.path()).unwrap(),
        )
        .observe(
            ReconciliationStrategy::Restore,
            &committed_after_adopt,
            &ReconciliationContext::Codex,
            compatibility(),
        )
        .unwrap();
        let (pending_before, pending_desired) = restored.prepared.durable_material().unwrap();
        restored.prepared.atomic_apply(home.path()).unwrap();
        restored.prepared.verify(home.path()).unwrap();

        let restored_document = fs::read_to_string(&config_path).unwrap();
        let restored_semantic: serde_json::Value =
            toml_edit::de::from_str(&restored_document).unwrap();
        let mut historical_semantic: serde_json::Value =
            toml_edit::de::from_str(historical).unwrap();
        historical_semantic["current_only"] = serde_json::json!({
            "unrelated_secret": "CURRENT_ONLY_UNRELATED_SECRET_97309"
        });
        assert!(
            restored_semantic == historical_semantic,
            "Restore did not recover the bounded historical semantics"
        );
        for decoration in [
            "# historical top decoration",
            "# historical model decoration",
            "# historical selector decoration",
            "# historical provider decoration",
        ] {
            assert!(restored_document.contains(decoration));
        }
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o640
        );

        let mut legacy_desired: serde_json::Value = serde_json::from_str(&pending_desired).unwrap();
        legacy_desired
            .as_object_mut()
            .unwrap()
            .remove("installed_file_state");
        let error = super::recover_pending_material(
            crate::control::protocol::Target::Codex,
            home.path(),
            &pending_before,
            &serde_json::to_string(&legacy_desired).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "recovery-required");
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o640
        );

        super::recover_pending_material(
            crate::control::protocol::Target::Codex,
            home.path(),
            &pending_before,
            &pending_desired,
        )
        .unwrap();
        let recovered_document = fs::read_to_string(&config_path).unwrap();
        let recovered_semantic: serde_json::Value =
            toml_edit::de::from_str(&recovered_document).unwrap();
        let external_semantic: serde_json::Value = toml_edit::de::from_str(&external).unwrap();
        assert!(
            recovered_semantic == external_semantic,
            "startup recovery did not restore the observed Codex semantics"
        );
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn pending_codex_restore_distinguishes_equal_semantics_with_different_modes() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let config_path = codec.config_path().to_owned();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        let historical = r#"model = "historical-model"
model_provider = "operator"
unrelated_setting = "preserve"

[model_providers.operator]
name = "Historical operator"
base_url = "https://historical.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer HISTORICAL_SECRET_97503" }
supports_websockets = false
operator_setting = "preserve"
"#;
        fs::write(&config_path, historical).unwrap();
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o640)).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_direct(
            "committed-model",
            "https://committed.example/v1",
            "COMMITTED_SECRET_97504",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        fs::write(&config_path, historical).unwrap();
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();

        let adopted = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Adopt,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();
        let crate::state::RecoveryPayload::Codex { before, desired } =
            adopted.prepared.adopted_recovery_payload()
        else {
            panic!("Codex Adopt must retain Codex recovery material")
        };
        let restored = TargetReconciliationAdapter::Codex(
            CodexConfigCodec::for_user_home(home.path()).unwrap(),
        )
        .observe(
            ReconciliationStrategy::Restore,
            &CommittedConfiguration::Codex {
                desired: (*desired).clone(),
                recovery_before: (*before).clone(),
            },
            &ReconciliationContext::Codex,
            compatibility(),
        )
        .unwrap();
        let (pending_before, pending_desired) = restored.prepared.durable_material().unwrap();
        restored.prepared.atomic_apply(home.path()).unwrap();
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o640
        );

        super::recover_pending_material(
            crate::control::protocol::Target::Codex,
            home.path(),
            &pending_before,
            &pending_desired,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let recovered: serde_json::Value =
            toml_edit::de::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let expected: serde_json::Value = toml_edit::de::from_str(historical).unwrap();
        assert!(
            recovered == expected,
            "startup rollback changed equal-semantics unrelated configuration"
        );

        let restored = TargetReconciliationAdapter::Codex(
            CodexConfigCodec::for_user_home(home.path()).unwrap(),
        )
        .observe(
            ReconciliationStrategy::Restore,
            &CommittedConfiguration::Codex {
                desired: (*desired).clone(),
                recovery_before: (*before).clone(),
            },
            &ReconciliationContext::Codex,
            compatibility(),
        )
        .unwrap();
        let (pending_before, pending_desired) = restored.prepared.durable_material().unwrap();
        restored.prepared.atomic_apply(home.path()).unwrap();
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = super::recover_pending_material(
            crate::control::protocol::Target::Codex,
            home.path(),
            &pending_before,
            &pending_desired,
        )
        .unwrap_err();
        assert_eq!(error.code(), "recovery-required");
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[cfg(unix)]
    #[test]
    fn pending_codex_restore_cas_preserves_semantic_and_mode_races() {
        for mutate_semantics in [true, false] {
            let home = TempDir::new().unwrap();
            let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
            let recovery_before = codec.inspect().unwrap();
            let committed = codec.desired_direct(
                "committed-model",
                "https://committed.example/v1",
                "COMMITTED_SECRET_97601",
            );
            codec.atomic_apply(&recovery_before, &committed).unwrap();
            let external = r#"model = "external-model"
model_provider = "external"
operator_note = "before-race"

[model_providers.external]
name = "External"
base_url = "https://external.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer EXTERNAL_SECRET_97602" }
supports_websockets = false
"#;
            fs::write(codec.config_path(), external).unwrap();
            fs::set_permissions(codec.config_path(), fs::Permissions::from_mode(0o600)).unwrap();

            let adopted = TargetReconciliationAdapter::Codex(codec)
                .observe(
                    ReconciliationStrategy::Adopt,
                    &CommittedConfiguration::Codex {
                        desired: committed,
                        recovery_before,
                    },
                    &ReconciliationContext::Codex,
                    compatibility(),
                )
                .unwrap();
            let crate::state::RecoveryPayload::Codex { before, desired } =
                adopted.prepared.adopted_recovery_payload()
            else {
                panic!("Codex Adopt must retain Codex recovery material")
            };
            let restored = TargetReconciliationAdapter::Codex(
                CodexConfigCodec::for_user_home(home.path()).unwrap(),
            )
            .observe(
                ReconciliationStrategy::Restore,
                &CommittedConfiguration::Codex {
                    desired: (*desired).clone(),
                    recovery_before: (*before).clone(),
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();
            restored.prepared.atomic_apply(home.path()).unwrap();
            let PreparedConfiguration::Codex {
                before,
                desired,
                restore: Some(restore),
                ..
            } = restored.prepared
            else {
                panic!("Codex Restore must carry one typed restore preparation")
            };

            let config_path = home.path().join(".codex/config.toml");
            let applied_bytes = fs::read(&config_path).unwrap();
            let mut raced_bytes = applied_bytes.clone();
            raced_bytes.extend_from_slice(b"\nexternal_race = true\n");
            let expected_raced_bytes = raced_bytes.clone();
            let hook_path = config_path.clone();
            let hook = move || {
                if mutate_semantics {
                    fs::write(&hook_path, &raced_bytes).unwrap();
                } else {
                    fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o640)).unwrap();
                }
            };
            let error = CodexConfigCodec::for_user_home(home.path())
                .unwrap()
                .restore_union_or_confirm_before_with_validation_hook(
                    &before,
                    &desired,
                    &restore.provider,
                    Some(&restore.installed_file),
                    &hook,
                )
                .unwrap_err();
            assert!(
                matches!(
                    error.code(),
                    "configuration-write-failed" | "recovery-required"
                ),
                "Restore race returned an unstable error code"
            );
            if mutate_semantics {
                assert!(
                    fs::read(&config_path).unwrap() == expected_raced_bytes,
                    "Restore overwrote an external semantic edit"
                );
            } else {
                assert!(
                    fs::read(&config_path).unwrap() == applied_bytes,
                    "Restore changed bytes during a mode-only race"
                );
                assert_eq!(
                    fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
                    0o640
                );
            }
        }
    }

    #[test]
    fn pending_codex_restore_reinstates_an_absent_immediate_before_file() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let config_path = codec.config_path().to_owned();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(
            &config_path,
            "model = \"historical-model\"\nmodel_provider = \"historical-provider\"\n",
        )
        .unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_direct(
            "committed-model",
            "https://committed.example/v1",
            "COMMITTED_SECRET_97505",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        fs::remove_file(&config_path).unwrap();

        let restored = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Restore,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();
        let (pending_before, pending_desired) = restored.prepared.durable_material().unwrap();
        super::recover_pending_material(
            crate::control::protocol::Target::Codex,
            home.path(),
            &pending_before,
            &pending_desired,
        )
        .unwrap();
        assert!(!config_path.exists());

        restored.prepared.atomic_apply(home.path()).unwrap();
        assert!(config_path.exists());
        super::recover_pending_material(
            crate::control::protocol::Target::Codex,
            home.path(),
            &pending_before,
            &pending_desired,
        )
        .unwrap();
        assert!(
            !config_path.exists(),
            "startup rollback retained an empty file for an absent pending before"
        );
    }

    #[test]
    fn startup_recovery_exactly_restores_a_pending_codex_reconciliation_write() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_direct(
            "committed-model",
            "https://committed.example/v1",
            "COMMITTED_SECRET_97104",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        fs::write(
            codec.config_path(),
            r#"model = "drifted-model"
model_provider = "operator"
[model_providers.operator]
name = "Operator"
base_url = "https://drifted.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer DRIFTED_SECRET_97105" }
supports_websockets = false
"#,
        )
        .unwrap();
        let before = codec.reconciliation_snapshot().unwrap().0;
        let before_json = serde_json::to_string(&before).unwrap();
        codec.atomic_apply(&before, &committed).unwrap();

        super::recover_pending_material(
            crate::control::protocol::Target::Codex,
            home.path(),
            &before_json,
            &serde_json::to_string(&committed).unwrap(),
        )
        .unwrap();

        let restored = CodexConfigCodec::for_user_home(home.path())
            .unwrap()
            .reconciliation_snapshot()
            .unwrap()
            .0;
        assert_eq!(restored.owned_fingerprint(), before.owned_fingerprint());
        assert_eq!(
            restored.unrelated_fingerprint(),
            before.unrelated_fingerprint()
        );
        for secret in ["COMMITTED_SECRET_97104", "DRIFTED_SECRET_97105"] {
            assert!(!format!("{restored:?}").contains(secret));
            assert!(!format!("{restored:?}").contains(&format!("{:?}", secret.as_bytes())));
        }
    }

    #[test]
    fn startup_recovery_rejects_corrupt_or_ambiguous_material_with_fixed_diagnostics() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let before = codec.inspect().unwrap();
        let desired = codec.desired_direct(
            "desired-model",
            "https://desired.example/v1",
            "DESIRED_SECRET_97111",
        );
        let before_json = serde_json::to_string(&before).unwrap();
        let desired_json = serde_json::to_string(&desired).unwrap();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(
            codec.config_path(),
            r#"model = "third-state"
model_provider = "third"
[model_providers.third]
name = "Third"
base_url = "https://third.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer THIRD_SECRET_97112" }
supports_websockets = false
"#,
        )
        .unwrap();

        for (before_material, desired_material) in [
            (before_json.as_str(), "CORRUPT_DESIRED_SENTINEL_97113"),
            (before_json.as_str(), desired_json.as_str()),
        ] {
            let problem = super::recover_pending_material(
                crate::control::protocol::Target::Codex,
                home.path(),
                before_material,
                desired_material,
            )
            .unwrap_err();
            assert_eq!(problem.code(), "recovery-required");
            let diagnostic = format!("{problem:?}\n{problem}");
            for sentinel in [
                "CORRUPT_DESIRED_SENTINEL_97113",
                "DESIRED_SECRET_97111",
                "THIRD_SECRET_97112",
            ] {
                assert!(!diagnostic.contains(sentinel));
                assert!(!diagnostic.contains(&format!("{:?}", sentinel.as_bytes())));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_codex_decor_and_unrelated_edits_do_not_change_owned_fingerprint() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_takeover("model-a", "http://127.0.0.1:43123/v1", "secret-a");
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        let auth_path = codec.config_path().with_file_name("auth.json");
        fs::write(&auth_path, b"sibling-auth-sentinel\n").unwrap();
        fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o640)).unwrap();
        let auth_before = shadow_fingerprint(&auth_path);
        let committed_configuration = CommittedConfiguration::Codex {
            desired: committed.clone(),
            recovery_before: recovery_before.clone(),
        };
        let baseline = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Reapply,
                &committed_configuration,
                &ReconciliationContext::Codex,
                compatibility(),
            )
            .unwrap();

        fs::write(
            home.path().join(".codex/config.toml"),
            r#"model    =    "model-a" # owned decor
model_provider = "muxvia_codex"
approval_policy = "operator-unrelated-change"

[model_providers.muxvia_codex]
name = "Muxvia"
base_url   =   "http://127.0.0.1:43123/v1" # more owned decor
wire_api = "responses"
http_headers = { "X-Muxvia-Routing-Credential" = "secret-a" }
supports_websockets = false
"#,
        )
        .unwrap();
        let changed = TargetReconciliationAdapter::Codex(
            CodexConfigCodec::for_user_home(home.path()).unwrap(),
        )
        .observe(
            ReconciliationStrategy::Reapply,
            &committed_configuration,
            &ReconciliationContext::Codex,
            compatibility(),
        )
        .unwrap();

        assert_eq!(
            changed.observation.owned_fingerprint,
            baseline.observation.owned_fingerprint
        );
        assert_ne!(
            changed.observation.unrelated_fingerprint,
            baseline.observation.unrelated_fingerprint
        );
        assert_eq!(changed.observation.changes, baseline.observation.changes);
        assert_eq!(shadow_fingerprint(&auth_path), auth_before);
    }

    #[test]
    fn reconciliation_mismatch_problem_diagnostics_are_stable_and_drop_probe_input() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_takeover("model-sentinel", "url-sentinel", "secret-sentinel");
        let error = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Reapply,
                &CommittedConfiguration::Codex {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Claude(claude_context(home.path())),
                ProbedCompatibility::new(
                    "raw-probe-output-sentinel".to_owned(),
                    CompatibilityClassification::Incompatible,
                ),
            )
            .unwrap_err();

        assert_eq!(error.code(), "recovery-required");
        assert_eq!(format!("{error}"), "recovery-required");
        assert_eq!(
            format!("{error:?}"),
            "ReconciliationProblem { code: \"recovery-required\" }"
        );
        assert!(!format!("{error:?}\n{error}").contains("raw-probe-output-sentinel"));
    }

    #[test]
    fn reconciliation_incompatible_compatibility_is_observed_without_writing() {
        let home = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(home.path()).unwrap();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(codec.config_path(), "approval_policy = \"never\"\n").unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_takeover(
            "committed-model-sentinel",
            "https://committed-url-sentinel.example/v1",
            "committed-credential-sentinel",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        fs::write(
            codec.config_path(),
            r#"profile = "operator-profile"
model = "observed-model-sentinel"
model_provider = "muxvia_codex"

[model_providers.muxvia_codex]
name = "Muxvia"
base_url = "https://observed-url-sentinel.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer observed-credential-sentinel" }
supports_websockets = false
"#,
        )
        .unwrap();
        let path = home.path().join(".codex/config.toml");
        let file_before = fs::read(&path).unwrap();
        let identity_before = codec
            .reconciliation_snapshot()
            .unwrap()
            .0
            .identity()
            .clone();

        let result = TargetReconciliationAdapter::Codex(codec)
            .observe(
                ReconciliationStrategy::Reapply,
                &CommittedConfiguration::Codex {
                    desired: committed.clone(),
                    recovery_before,
                },
                &ReconciliationContext::Codex,
                ProbedCompatibility::new(
                    "codex-cli 0.0.0".to_owned(),
                    CompatibilityClassification::Incompatible,
                ),
            )
            .expect("read-only observation must retain incompatible classification");

        assert_eq!(
            result.observation.compatibility.classification(),
            CompatibilityClassification::Incompatible
        );
        assert_eq!(
            result.observation.compatibility.version(),
            "codex-cli 0.0.0"
        );
        assert_eq!(result.observation.shadows, vec![ShadowSource::CodexProfile]);
        assert_eq!(
            result.observation.changes,
            vec![
                ReconciliationFieldChange {
                    field: ReconciliationField::Provider,
                    state: ReconciliationFieldState::Unchanged,
                },
                ReconciliationFieldChange {
                    field: ReconciliationField::Credential,
                    state: ReconciliationFieldState::Unchanged,
                },
                ReconciliationFieldChange {
                    field: ReconciliationField::CurrentProvider,
                    state: ReconciliationFieldState::Unchanged,
                },
                ReconciliationFieldChange {
                    field: ReconciliationField::ActivatedSnapshot,
                    state: ReconciliationFieldState::Unchanged,
                },
                ReconciliationFieldChange {
                    field: ReconciliationField::Takeover,
                    state: ReconciliationFieldState::Unchanged,
                },
            ]
        );
        match &result.prepared {
            PreparedConfiguration::Codex {
                before, desired, ..
            } => {
                assert_ne!(before.as_desired_like(&committed), committed);
                assert_eq!(desired, &committed);
            }
            PreparedConfiguration::Claude { .. } => panic!("wrong prepared target"),
        }
        let diagnostic = format!("{result:?}");
        for sentinel in [
            "committed-model-sentinel",
            "committed-url-sentinel",
            "committed-credential-sentinel",
            "observed-model-sentinel",
            "observed-url-sentinel",
            "observed-credential-sentinel",
        ] {
            assert!(!diagnostic.contains(sentinel));
            assert!(!diagnostic.contains(&format!("{:?}", sentinel.as_bytes())));
        }
        assert_eq!(fs::read(path).unwrap(), file_before);
        assert_eq!(
            CodexConfigCodec::for_user_home(home.path())
                .unwrap()
                .reconciliation_snapshot()
                .unwrap()
                .0
                .identity(),
            &identity_before
        );
    }

    #[test]
    fn reconciliation_claude_strategies_preserve_historical_ownership_and_redact_values() {
        for ownership in [
            ClaudeConfigOwnership::LegacyThree,
            ClaudeConfigOwnership::FourField,
        ] {
            let home = TempDir::new().unwrap();
            let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
            fs::create_dir_all(codec.settings_path().parent().unwrap()).unwrap();
            fs::write(
                codec.settings_path(),
                r#"{"env":{"ANTHROPIC_API_KEY":"prior-api-key-sentinel","KEEP":"prior-unrelated-sentinel"},"permissions":{}}"#,
            )
            .unwrap();
            let recovery_before = codec.inspect_with_ownership(ownership).unwrap();
            let committed = codec.desired_takeover_with_ownership(
                "committed-model-sentinel",
                "http://127.0.0.1:43124/committed-url-sentinel",
                "committed-credential-sentinel",
                ownership,
            );
            codec.atomic_apply(&recovery_before, &committed).unwrap();
            fs::write(
                codec.settings_path(),
                r#"{"env":{"ANTHROPIC_BASE_URL":"https://observed-url-sentinel.example","ANTHROPIC_AUTH_TOKEN":"observed-credential-sentinel","ANTHROPIC_MODEL":"observed-model-sentinel","ANTHROPIC_API_KEY":"observed-api-key-sentinel","KEEP":"observed-unrelated-sentinel"},"permissions":{}}"#,
            )
            .unwrap();

            for strategy in [
                ReconciliationStrategy::Adopt,
                ReconciliationStrategy::Reapply,
                ReconciliationStrategy::Restore,
            ] {
                let result = TargetReconciliationAdapter::Claude(
                    ClaudeConfigCodec::for_user_home(home.path()).unwrap(),
                )
                .observe(
                    strategy,
                    &CommittedConfiguration::Claude {
                        desired: committed.clone(),
                        recovery_before: recovery_before.clone(),
                    },
                    &ReconciliationContext::Claude(claude_context(home.path())),
                    claude_compatibility(),
                )
                .unwrap();
                assert_eq!(result.observation.shadows, Vec::<ShadowSource>::new());
                match (&result.prepared, strategy) {
                    (
                        PreparedConfiguration::Claude {
                            before, desired, ..
                        },
                        ReconciliationStrategy::Adopt,
                    ) => assert_eq!(before.as_adopted_direct(), *desired),
                    (
                        PreparedConfiguration::Claude { desired, .. },
                        ReconciliationStrategy::Reapply,
                    ) => assert_eq!(desired, &committed),
                    (
                        PreparedConfiguration::Claude { desired, .. },
                        ReconciliationStrategy::Restore,
                    ) => assert_eq!(desired, &recovery_before.as_desired_like(&committed)),
                    _ => panic!("wrong prepared target"),
                }
                let serialized = serde_json::to_string(&result.observation.changes).unwrap();
                let panic_text = panic_message(
                    std::panic::catch_unwind(|| panic!("{result:?}"))
                        .expect_err("diagnostic panic mutation did not panic"),
                );
                let diagnostic = format!("{result:?}\n{serialized}\n{panic_text}");
                for sentinel in [
                    "prior-api-key-sentinel",
                    "prior-unrelated-sentinel",
                    "committed-model-sentinel",
                    "committed-url-sentinel",
                    "committed-credential-sentinel",
                    "observed-model-sentinel",
                    "observed-url-sentinel",
                    "observed-credential-sentinel",
                    "observed-api-key-sentinel",
                    "observed-unrelated-sentinel",
                ] {
                    assert!(!diagnostic.contains(sentinel));
                    assert!(!diagnostic.contains(&format!("{:?}", sentinel.as_bytes())));
                }
                assert_eq!(result.observation.owned_fingerprint.len(), 64);
                assert_eq!(result.observation.unrelated_fingerprint.len(), 64);
            }
        }
    }

    #[test]
    fn reconciliation_claude_api_key_ownership_is_exactly_legacy_three_or_current_four() {
        let home = TempDir::new().unwrap();
        let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
        fs::create_dir_all(codec.settings_path().parent().unwrap()).unwrap();
        fs::write(
            codec.settings_path(),
            r#"{"env":{"ANTHROPIC_BASE_URL":"url","ANTHROPIC_AUTH_TOKEN":"token","ANTHROPIC_MODEL":"model","ANTHROPIC_API_KEY":"api-one","KEEP":"same"}}"#,
        )
        .unwrap();
        let legacy_before = codec
            .inspect_with_ownership(ClaudeConfigOwnership::LegacyThree)
            .unwrap();
        let current_before = codec
            .inspect_with_ownership(ClaudeConfigOwnership::FourField)
            .unwrap();
        fs::write(
            codec.settings_path(),
            r#"{"env":{"ANTHROPIC_BASE_URL":"url","ANTHROPIC_AUTH_TOKEN":"token","ANTHROPIC_MODEL":"model","ANTHROPIC_API_KEY":"api-two","KEEP":"same"}}"#,
        )
        .unwrap();
        let legacy_after = codec
            .inspect_with_ownership(ClaudeConfigOwnership::LegacyThree)
            .unwrap();
        let current_after = codec
            .inspect_with_ownership(ClaudeConfigOwnership::FourField)
            .unwrap();

        assert_eq!(
            legacy_after.owned_fingerprint(),
            legacy_before.owned_fingerprint()
        );
        assert_ne!(
            legacy_after.unrelated_fingerprint(),
            legacy_before.unrelated_fingerprint()
        );
        assert_ne!(
            current_after.owned_fingerprint(),
            current_before.owned_fingerprint()
        );
        assert_eq!(
            current_after.unrelated_fingerprint(),
            current_before.unrelated_fingerprint()
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_claude_projects_selectors_and_host_are_closed_shadows() {
        for selector in [
            ClaudeBlockingSelector::Bedrock,
            ClaudeBlockingSelector::Vertex,
            ClaudeBlockingSelector::Foundry,
            ClaudeBlockingSelector::Mantle,
            ClaudeBlockingSelector::AnthropicAws,
        ] {
            let home = TempDir::new().unwrap();
            let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
            let recovery_before = codec.inspect().unwrap();
            let committed = codec.desired_takeover("m", "http://127.0.0.1:9", "c");
            codec.atomic_apply(&recovery_before, &committed).unwrap();
            let mut document: serde_json::Value =
                serde_json::from_slice(&fs::read(codec.settings_path()).unwrap()).unwrap();
            document["env"][selector.as_str()] = serde_json::json!(true);
            fs::write(
                codec.settings_path(),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
            let before = fs::read(codec.settings_path()).unwrap();

            let result = TargetReconciliationAdapter::Claude(codec)
                .observe(
                    ReconciliationStrategy::Reapply,
                    &CommittedConfiguration::Claude {
                        desired: committed,
                        recovery_before,
                    },
                    &ReconciliationContext::Claude(claude_context(home.path())),
                    claude_compatibility(),
                )
                .unwrap();

            assert_eq!(
                result.observation.shadows,
                vec![ShadowSource::ClaudeSelector(selector)]
            );
            assert_eq!(
                fs::read(home.path().join(".claude/settings.json")).unwrap(),
                before
            );
        }

        let home = TempDir::new().unwrap();
        let project = home.path().join("project");
        fs::create_dir_all(project.join(".claude")).unwrap();
        let managed = home.path().join("managed-settings.json");
        let codec = ClaudeConfigCodec::for_user_home_with_managed_settings(
            home.path(),
            vec![managed.clone()],
        )
        .unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_takeover("m", "http://127.0.0.1:9", "c");
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        let project_settings = project.join(".claude/settings.json");
        let local_settings = project.join(".claude/settings.local.json");
        for path in [&managed, &project_settings, &local_settings] {
            fs::write(path, r#"{"env":{"ANTHROPIC_MODEL":"shadow-sentinel"}}"#).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o640)).unwrap();
        }
        let before = [
            shadow_fingerprint(&managed),
            shadow_fingerprint(&project_settings),
            shadow_fingerprint(&local_settings),
        ];

        let result = TargetReconciliationAdapter::Claude(codec)
            .observe(
                ReconciliationStrategy::Reapply,
                &CommittedConfiguration::Claude {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Claude(claude_context(&project)),
                claude_compatibility(),
            )
            .unwrap();

        assert_eq!(
            result.observation.shadows,
            vec![
                ShadowSource::ClaudeManaged,
                ShadowSource::ClaudeShared,
                ShadowSource::ClaudeLocal,
            ]
        );
        assert_eq!(
            [
                shadow_fingerprint(&managed),
                shadow_fingerprint(&project_settings),
                shadow_fingerprint(&local_settings),
            ],
            before
        );

        let mut host_context = claude_context(&project);
        host_context.host_managed_state = ClaudeHostManagedState::Managed;
        host_context.blocking_selector = Some(ClaudeBlockingSelector::HostManaged);
        let result = TargetReconciliationAdapter::Claude(
            ClaudeConfigCodec::for_user_home(home.path()).unwrap(),
        )
        .observe(
            ReconciliationStrategy::Reapply,
            &CommittedConfiguration::Claude {
                desired: codec_desired_for_host(home.path()),
                recovery_before: ClaudeConfigCodec::for_user_home(home.path())
                    .unwrap()
                    .inspect()
                    .unwrap(),
            },
            &ReconciliationContext::Claude(host_context),
            claude_compatibility(),
        );
        assert!(result.is_ok());
        assert!(
            result
                .unwrap()
                .observation
                .shadows
                .contains(&ShadowSource::ClaudeHostManaged)
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_claude_directory_symlink_does_not_shadow_its_managed_file() {
        let home = TempDir::new().unwrap();
        let actual_configuration_home = home.path().join("actual-claude-home");
        fs::create_dir_all(&actual_configuration_home).unwrap();
        symlink(&actual_configuration_home, home.path().join(".claude")).unwrap();
        let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_takeover(
            "model-sentinel",
            "http://127.0.0.1:43124/url-sentinel",
            "credential-sentinel",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        let settings_path = actual_configuration_home.join("settings.json");
        let file_before = shadow_fingerprint(&settings_path);

        let result = TargetReconciliationAdapter::Claude(codec)
            .observe(
                ReconciliationStrategy::Reapply,
                &CommittedConfiguration::Claude {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Claude(claude_context(home.path())),
                claude_compatibility(),
            )
            .unwrap();

        assert_eq!(result.observation.shadows, Vec::<ShadowSource>::new());
        assert_eq!(shadow_fingerprint(&settings_path), file_before);
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_claude_project_file_symlink_to_managed_file_is_shared_shadow() {
        let home = TempDir::new().unwrap();
        let project = home.path().join("project");
        fs::create_dir_all(project.join(".claude")).unwrap();
        let codec = ClaudeConfigCodec::for_user_home(home.path()).unwrap();
        let recovery_before = codec.inspect().unwrap();
        let committed = codec.desired_takeover(
            "model-sentinel",
            "http://127.0.0.1:43124/url-sentinel",
            "credential-sentinel",
        );
        codec.atomic_apply(&recovery_before, &committed).unwrap();
        let settings_path = codec.settings_path().to_owned();
        let project_settings = project.join(".claude/settings.json");
        symlink(&settings_path, &project_settings).unwrap();
        let file_before = shadow_fingerprint(&settings_path);
        let link_before = fs::symlink_metadata(&project_settings).unwrap();
        let link_target_before = fs::read_link(&project_settings).unwrap();

        let result = TargetReconciliationAdapter::Claude(codec)
            .observe(
                ReconciliationStrategy::Reapply,
                &CommittedConfiguration::Claude {
                    desired: committed,
                    recovery_before,
                },
                &ReconciliationContext::Claude(claude_context(&project)),
                claude_compatibility(),
            )
            .unwrap();

        assert_eq!(result.observation.shadows, vec![ShadowSource::ClaudeShared]);
        assert_eq!(shadow_fingerprint(&settings_path), file_before);
        let link_after = fs::symlink_metadata(&project_settings).unwrap();
        assert_eq!(
            fs::read_link(&project_settings).unwrap(),
            link_target_before
        );
        assert_eq!(
            link_after.permissions().mode() & 0o777,
            link_before.permissions().mode() & 0o777
        );
        assert_eq!(
            link_after.modified().unwrap(),
            link_before.modified().unwrap()
        );
    }

    fn codec_desired_for_host(home: &std::path::Path) -> crate::claude::DesiredClaudeState {
        ClaudeConfigCodec::for_user_home(home)
            .unwrap()
            .desired_takeover("m", "http://127.0.0.1:9", "c")
    }
}
