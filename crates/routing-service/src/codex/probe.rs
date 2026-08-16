use std::{
    fmt,
    future::Future,
    path::Path,
    pin::Pin,
    process::{Command, Stdio},
};

use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::control::protocol::CompatibilityClassification;

const TESTED_CODEX_VERSION: &str = "codex-cli 0.106.0";

pub trait CodexProbe: Send + Sync {
    fn probe(&self, executable: &Path) -> Result<CodexCapability, CodexProblem>;

    fn probe_cancellable<'a>(
        &'a self,
        executable: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexCapability {
    Tested { version: String },
    UnknownCompatible { version: String, warning: String },
}

impl CodexCapability {
    pub fn version(&self) -> &str {
        match self {
            Self::Tested { version } | Self::UnknownCompatible { version, .. } => version,
        }
    }

    pub fn classification(&self) -> CompatibilityClassification {
        match self {
            Self::Tested { .. } => CompatibilityClassification::Tested,
            Self::UnknownCompatible { .. } => CompatibilityClassification::UnknownCompatible,
        }
    }
}

#[derive(Clone)]
pub struct CodexProblem {
    code: &'static str,
    path: Option<std::path::PathBuf>,
    version: Option<String>,
    correlation_id: Uuid,
}

impl CodexProblem {
    pub(crate) fn new(code: &'static str, path: Option<&Path>) -> Self {
        Self {
            code,
            path: path.map(Path::to_path_buf),
            version: None,
            correlation_id: Uuid::new_v4(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn with_version(mut self, version: String) -> Self {
        self.version = Some(version);
        self
    }
}

impl fmt::Debug for CodexProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexProblem")
            .field("code", &self.code)
            .field("path", &self.path)
            .field("correlation_id", &self.correlation_id)
            .finish()
    }
}

impl fmt::Display for CodexProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (correlation {})",
            self.code, self.correlation_id
        )
    }
}

impl std::error::Error for CodexProblem {}

pub struct CommandCodexProbe;

impl CodexProbe for CommandCodexProbe {
    fn probe(&self, executable: &Path) -> Result<CodexCapability, CodexProblem> {
        if !executable.is_absolute() {
            return Err(CodexProblem::new(
                "incompatible-target-cli",
                Some(executable),
            ));
        }
        let version = parse_version(&run_read_only(executable, "--version")?)
            .ok_or_else(|| CodexProblem::new("incompatible-target-cli", Some(executable)))?;
        let help = run_read_only(executable, "--help")
            .map_err(|problem| problem.with_version(version.clone()))?;
        let normalized_help = help.to_ascii_lowercase();
        if !normalized_help.contains("usage:")
            || !normalized_help.contains("codex")
            || !normalized_help.contains("--config")
        {
            return Err(
                CodexProblem::new("incompatible-target-cli", Some(executable))
                    .with_version(version),
            );
        }
        if version == TESTED_CODEX_VERSION {
            Ok(CodexCapability::Tested { version })
        } else {
            Ok(CodexCapability::UnknownCompatible {
                warning: format!("Untested Codex CLI version: {version}"),
                version,
            })
        }
    }

    fn probe_cancellable<'a>(
        &'a self,
        executable: &'a Path,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>> {
        Box::pin(async move {
            if !executable.is_absolute() {
                return Err(CodexProblem::new(
                    "incompatible-target-cli",
                    Some(executable),
                ));
            }
            let version = parse_version(
                &run_read_only_cancellable(executable, "--version", cancellation.clone()).await?,
            )
            .ok_or_else(|| CodexProblem::new("incompatible-target-cli", Some(executable)))?;
            let help = run_read_only_cancellable(executable, "--help", cancellation)
                .await
                .map_err(|problem| problem.with_version(version.clone()))?;
            let normalized_help = help.to_ascii_lowercase();
            if !normalized_help.contains("usage:")
                || !normalized_help.contains("codex")
                || !normalized_help.contains("--config")
            {
                return Err(
                    CodexProblem::new("incompatible-target-cli", Some(executable))
                        .with_version(version),
                );
            }
            if version == TESTED_CODEX_VERSION {
                Ok(CodexCapability::Tested { version })
            } else {
                Ok(CodexCapability::UnknownCompatible {
                    warning: format!("Untested Codex CLI version: {version}"),
                    version,
                })
            }
        })
    }
}

fn parse_version(output: &str) -> Option<String> {
    let mut lines = output.lines();
    let version = lines.next()?.trim();
    if version.is_empty() || lines.next().is_some() {
        return None;
    }
    let number = version.strip_prefix("codex-cli ")?;
    if is_version_token(number) {
        Some(version.to_owned())
    } else {
        None
    }
}

fn is_version_token(number: &str) -> bool {
    let core_end = number.find(['-', '+']).unwrap_or(number.len());
    let core = &number[..core_end];
    core.split('.').count() >= 2
        && core
            .split('.')
            .all(|segment| !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()))
        && number
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && number.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+')
        })
}

fn run_read_only(executable: &Path, argument: &str) -> Result<String, CodexProblem> {
    let output = Command::new(executable)
        .arg(argument)
        .output()
        .map_err(|_| CodexProblem::new("incompatible-target-cli", Some(executable)))?;
    if !output.status.success() {
        return Err(CodexProblem::new(
            "incompatible-target-cli",
            Some(executable),
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| CodexProblem::new("incompatible-target-cli", Some(executable)))
}

async fn run_read_only_cancellable(
    executable: &Path,
    argument: &str,
    cancellation: CancellationToken,
) -> Result<String, CodexProblem> {
    let mut command = tokio::process::Command::new(executable);
    command
        .arg(argument)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| CodexProblem::new("incompatible-target-cli", Some(executable)))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| CodexProblem::new("incompatible-target-cli", Some(executable)))?;
    let mut reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let status = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            reader.abort();
            let _ = reader.await;
            return Err(CodexProblem::new("probe-cancelled", Some(executable)));
        }
        status = child.wait() => status,
    };
    let status = match status {
        Ok(status) => status,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            reader.abort();
            let _ = reader.await;
            return Err(CodexProblem::new(
                "incompatible-target-cli",
                Some(executable),
            ));
        }
    };
    let output = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            reader.abort();
            let _ = reader.await;
            return Err(CodexProblem::new("probe-cancelled", Some(executable)));
        }
        output = &mut reader => output,
    }
    .map_err(|_| CodexProblem::new("incompatible-target-cli", Some(executable)))?
    .map_err(|_| CodexProblem::new("incompatible-target-cli", Some(executable)))?;
    if !status.success() {
        return Err(CodexProblem::new(
            "incompatible-target-cli",
            Some(executable),
        ));
    }
    String::from_utf8(output)
        .map_err(|_| CodexProblem::new("incompatible-target-cli", Some(executable)))
}
