use std::{fmt, path::Path, process::Command};

use uuid::Uuid;

use crate::control::protocol::ClaudeBlockingSelector;
use crate::control::protocol::CompatibilityClassification;

const TESTED_CLAUDE_VERSION: &str = "2.1.37 (Claude Code)";

pub trait ClaudeProbe: Send + Sync {
    fn probe(&self, executable: &Path) -> Result<ClaudeCapability, ClaudeProblem>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeCapability {
    Tested { version: String },
    UnknownCompatible { version: String, warning: String },
}

impl ClaudeCapability {
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
pub struct ClaudeProblem {
    code: &'static str,
    path: Option<std::path::PathBuf>,
    source: Option<&'static str>,
    selector: Option<ClaudeBlockingSelector>,
    correlation_id: Uuid,
}

impl ClaudeProblem {
    pub(crate) fn new(code: &'static str, path: Option<&Path>) -> Self {
        Self {
            code,
            path: path.map(Path::to_path_buf),
            source: None,
            selector: None,
            correlation_id: Uuid::new_v4(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn with_source(mut self, source: &'static str) -> Self {
        self.source = Some(source);
        self
    }

    pub(crate) fn with_selector(mut self, selector: ClaudeBlockingSelector) -> Self {
        self.selector = Some(selector);
        self
    }

    pub(crate) fn source(&self) -> Option<&'static str> {
        self.source
    }

    pub(crate) fn selector(&self) -> Option<ClaudeBlockingSelector> {
        self.selector
    }
}

impl fmt::Debug for ClaudeProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeProblem")
            .field("code", &self.code)
            .field("path", &self.path)
            .field("source", &self.source)
            .field("selector", &self.selector)
            .field("correlation_id", &self.correlation_id)
            .finish()
    }
}

impl fmt::Display for ClaudeProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (correlation {})",
            self.code, self.correlation_id
        )
    }
}

impl std::error::Error for ClaudeProblem {}

pub struct CommandClaudeProbe;

impl ClaudeProbe for CommandClaudeProbe {
    fn probe(&self, executable: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
        if !executable.is_absolute() {
            return Err(ClaudeProblem::new(
                "incompatible-target-cli",
                Some(executable),
            ));
        }
        let version = run_read_only(executable, "--version")?;
        let help = run_read_only(executable, "--help")?;
        let normalized_help = help.to_ascii_lowercase();
        if !normalized_help.contains("usage:")
            || !normalized_help.contains("claude")
            || !normalized_help.contains("--settings")
            || !normalized_help.contains("--model")
        {
            return Err(ClaudeProblem::new(
                "incompatible-target-cli",
                Some(executable),
            ));
        }
        let version = parse_version(&version)
            .ok_or_else(|| ClaudeProblem::new("incompatible-target-cli", Some(executable)))?;
        if version == TESTED_CLAUDE_VERSION {
            Ok(ClaudeCapability::Tested { version })
        } else {
            Ok(ClaudeCapability::UnknownCompatible {
                warning: format!("Untested Claude Code version: {version}"),
                version,
            })
        }
    }
}

fn parse_version(output: &str) -> Option<String> {
    let mut lines = output.lines();
    let version = lines.next()?.trim();
    if version.is_empty() || lines.next().is_some() {
        return None;
    }
    let number = version.strip_suffix(" (Claude Code)")?;
    if number.contains('.')
        && number.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+')
        })
    {
        Some(version.to_owned())
    } else {
        None
    }
}

fn run_read_only(executable: &Path, argument: &str) -> Result<String, ClaudeProblem> {
    let output = Command::new(executable)
        .arg(argument)
        .output()
        .map_err(|_| ClaudeProblem::new("incompatible-target-cli", Some(executable)))?;
    if !output.status.success() {
        return Err(ClaudeProblem::new(
            "incompatible-target-cli",
            Some(executable),
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| ClaudeProblem::new("incompatible-target-cli", Some(executable)))
}
