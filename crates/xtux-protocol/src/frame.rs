//! Reading and writing protocol v2 frames.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{MAGIC, MAX_FRAME};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(u32),
    #[error("not a protocol v2 frame")]
    BadMagic,
    #[error("invalid message: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encodes a message into a complete frame.
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(message)?;
    let len = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend(MAGIC.to_le_bytes());
    frame.extend(len.to_le_bytes());
    frame.extend(payload);
    Ok(frame)
}

pub async fn write<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<(), FrameError> {
    writer.write_all(&encode(message)?).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads the rest of a frame whose magic was already consumed (the daemon reads the magic to pick the protocol).
pub async fn read_after_magic<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<T, FrameError> {
    let len = reader.read_u32_le().await?;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut payload = vec![0; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

/// Reads a whole frame. `Ok(None)` when the peer closed the connection between frames.
pub async fn read<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<Option<T>, FrameError> {
    let mut magic = [0u8; 4];
    match reader.read_exact(&mut magic).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    if u32::from_le_bytes(magic) != MAGIC {
        return Err(FrameError::BadMagic);
    }
    read_after_magic(reader).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientMessage, Request};

    #[tokio::test]
    async fn round_trip() {
        let msg = ClientMessage {
            id: 1,
            request: Request::Ping,
        };
        let frame = encode(&msg).unwrap();
        assert_eq!(&frame[..4], b"XTX2");
        let mut reader = &frame[..];
        let back: Option<ClientMessage> = read(&mut reader).await.unwrap();
        assert_eq!(back, Some(msg));
        let end: Option<ClientMessage> = read(&mut reader).await.unwrap();
        assert_eq!(end, None);
    }

    #[tokio::test]
    async fn rejects_oversized_frames() {
        let mut frame = MAGIC.to_le_bytes().to_vec();
        frame.extend((MAX_FRAME + 1).to_le_bytes());
        let mut reader = &frame[..];
        let err = read::<_, ClientMessage>(&mut reader).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }
}
