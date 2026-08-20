use std::{fmt, path::PathBuf, process::Stdio, time::Duration};

use serde::Deserialize;
use tokio::process::Command;

const MAX_METADATA_BYTES: usize = 4096;
const METADATA_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct PreparedHandover {
    pub(crate) candidate_path: PathBuf,
    pub(crate) release: String,
}

impl fmt::Debug for PreparedHandover {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedHandover")
            .field("candidate_path", &"<redacted>")
            .field("release", &self.release)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandoverProblem {
    InvalidCandidate,
    ReleaseMismatch,
    ProtocolMismatch,
}

impl HandoverProblem {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InvalidCandidate => "invalid-handover-candidate",
            Self::ReleaseMismatch => "handover-release-mismatch",
            Self::ProtocolMismatch => "protocol-mismatch",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::InvalidCandidate => "Routing Service candidate is invalid",
            Self::ReleaseMismatch => "Routing Service candidate release does not match",
            Self::ProtocolMismatch => "Routing Service candidate protocol is incompatible",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LifecycleMetadata {
    product: String,
    release: String,
    rpc: LifecycleRpc,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleRpc {
    major: u16,
    minor: u16,
}

pub(crate) async fn probe_candidate(
    candidate_path: PathBuf,
    expected_release: &str,
) -> Result<PreparedHandover, HandoverProblem> {
    if !candidate_path.is_absolute() || expected_release.is_empty() {
        return Err(HandoverProblem::InvalidCandidate);
    }
    let candidate_path =
        std::fs::canonicalize(candidate_path).map_err(|_| HandoverProblem::InvalidCandidate)?;
    if !candidate_path.is_file() {
        return Err(HandoverProblem::InvalidCandidate);
    }
    let mut command = Command::new(&candidate_path);
    command
        .arg("--lifecycle-metadata")
        .env_remove("HOME")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(METADATA_PROBE_TIMEOUT, command.output())
        .await
        .map_err(|_| HandoverProblem::InvalidCandidate)?
        .map_err(|_| HandoverProblem::InvalidCandidate)?;
    if !output.status.success()
        || !output.stderr.is_empty()
        || output.stdout.len() > MAX_METADATA_BYTES
    {
        return Err(HandoverProblem::InvalidCandidate);
    }
    let metadata: LifecycleMetadata =
        serde_json::from_slice(&output.stdout).map_err(|_| HandoverProblem::InvalidCandidate)?;
    if metadata.product != "muxvia-routing" {
        return Err(HandoverProblem::InvalidCandidate);
    }
    if metadata.release != expected_release {
        return Err(HandoverProblem::ReleaseMismatch);
    }
    if metadata.rpc.major != 1 {
        return Err(HandoverProblem::ProtocolMismatch);
    }
    let _compatible_minor = metadata.rpc.minor;
    Ok(PreparedHandover {
        candidate_path,
        release: metadata.release,
    })
}
