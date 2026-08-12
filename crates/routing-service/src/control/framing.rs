use std::str;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::protocol::FRAME_LIMIT;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("end-of-stream")]
    EndOfStream,
    #[error("frame-too-large")]
    FrameTooLarge,
    #[error("invalid-utf8")]
    InvalidUtf8,
    #[error("invalid-json")]
    InvalidJson,
    #[error("unexpected-eof")]
    UnexpectedEof,
    #[error("io-error")]
    Io,
}

pub async fn read_frame<R>(reader: &mut R) -> Result<Value, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0; 4];
    read_prefix(reader, &mut prefix).await?;
    let length = u32::from_be_bytes(prefix);
    if length > FRAME_LIMIT {
        return Err(FrameError::FrameTooLarge);
    }

    let mut body = vec![0; length as usize];
    read_exact(reader, &mut body).await?;
    let text = str::from_utf8(&body).map_err(|_| FrameError::InvalidUtf8)?;
    serde_json::from_str(text).map_err(|_| FrameError::InvalidJson)
}

async fn read_prefix<R>(reader: &mut R, prefix: &mut [u8; 4]) -> Result<(), FrameError>
where
    R: AsyncRead + Unpin,
{
    let first = reader.read(prefix).await.map_err(|_| FrameError::Io)?;
    if first == 0 {
        return Err(FrameError::EndOfStream);
    }
    read_exact(reader, &mut prefix[first..]).await
}

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(value).map_err(|_| FrameError::InvalidJson)?;
    if body.len() > FRAME_LIMIT as usize {
        return Err(FrameError::FrameTooLarge);
    }
    writer
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .map_err(|_| FrameError::Io)?;
    writer.write_all(&body).await.map_err(|_| FrameError::Io)
}

async fn read_exact<R>(reader: &mut R, buffer: &mut [u8]) -> Result<(), FrameError>
where
    R: AsyncRead + Unpin,
{
    reader
        .read_exact(buffer)
        .await
        .map(|_| ())
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::UnexpectedEof => FrameError::UnexpectedEof,
            _ => FrameError::Io,
        })
}
