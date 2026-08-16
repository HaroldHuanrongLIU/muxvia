use std::{path::Path, process::Stdio};

use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub(crate) enum ProbeProcessError {
    Cancelled,
    Failed,
}

pub(crate) async fn run_cancellable(
    executable: &Path,
    argument: &str,
    cancellation: CancellationToken,
) -> Result<Vec<u8>, ProbeProcessError> {
    let mut command = tokio::process::Command::new(executable);
    command
        .arg(argument)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| ProbeProcessError::Failed)?;
    let mut stdout = child
        .stdout
        .take()
        .expect("piped probe stdout is available");
    let mut reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let status = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            kill_and_reap(&mut child).await;
            abort_and_await(reader).await;
            return Err(ProbeProcessError::Cancelled);
        }
        status = child.wait() => status,
    };
    let status = match status {
        Ok(status) => status,
        Err(_) => {
            kill_and_reap(&mut child).await;
            abort_and_await(reader).await;
            return Err(ProbeProcessError::Failed);
        }
    };
    let output = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            abort_and_await(reader).await;
            return Err(ProbeProcessError::Cancelled);
        }
        output = &mut reader => output,
    }
    .map_err(|_| ProbeProcessError::Failed)?
    .map_err(|_| ProbeProcessError::Failed)?;
    if !status.success() {
        return Err(ProbeProcessError::Failed);
    }
    Ok(output)
}

async fn kill_and_reap(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn abort_and_await(reader: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>) {
    reader.abort();
    let _ = reader.await;
}
