use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::IpcError;
use crate::protocol::MAX_FRAME_LEN;

// ── Sync codec (used by fff-mcp's blocking EngineClient) ─────────────────────

/// Synchronous write of a length-prefixed bincode message.
pub fn write_message_sync<W: Write, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), IpcError> {
    let payload = bincode::serialize(value).map_err(IpcError::Encode)?;
    let len = payload.len() as u32;
    writer.write_all(&len.to_le_bytes()).map_err(IpcError::Io)?;
    writer.write_all(&payload).map_err(IpcError::Io)?;
    Ok(())
}

/// Synchronous read of a length-prefixed bincode message.
pub fn read_message_sync<R: Read, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<T, IpcError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(IpcError::Io)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).map_err(IpcError::Io)?;

    bincode::deserialize(&payload).map_err(IpcError::Decode)
}

/// Write a length-prefixed bincode message.
///
/// Frame layout: `[ 4-byte LE u32 payload length ][ payload bytes ]`
pub async fn write_message<W, T>(writer: &mut W, value: &T) -> Result<(), IpcError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = bincode::serialize(value).map_err(IpcError::Encode)?;
    let len = payload.len() as u32;
    writer
        .write_all(&len.to_le_bytes())
        .await
        .map_err(IpcError::Io)?;
    writer.write_all(&payload).await.map_err(IpcError::Io)?;
    Ok(())
}

/// Read a length-prefixed bincode message.
///
/// Returns `Err(IpcError::Io)` wrapping `UnexpectedEof` when the stream ends
/// before a full frame is received (truncated data or clean EOF mid-frame).
pub async fn read_message<R, T>(reader: &mut R) -> Result<T, IpcError>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(IpcError::Io)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(IpcError::Io)?;

    bincode::deserialize(&payload).map_err(IpcError::Decode)
}

// ── JSON codec (versioned protocol envelope) ─────────────────────────────────
//
// Reuses the SAME `[u32-LE len][payload]` framing as the bincode functions, but
// serializes/deserializes via serde_json and enforces MAX_FRAME_LEN on the read
// path so a hostile/garbled length can't trigger an unbounded allocation (KTD9).

/// Synchronous write of a length-prefixed JSON message.
pub fn write_json_message_sync<W: Write, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), IpcError> {
    let payload = serde_json::to_vec(value).map_err(IpcError::JsonEncode)?;
    let len = payload.len() as u32;
    writer.write_all(&len.to_le_bytes()).map_err(IpcError::Io)?;
    writer.write_all(&payload).map_err(IpcError::Io)?;
    Ok(())
}

/// Synchronous read of a length-prefixed JSON message. Rejects an over-length
/// declared frame before allocating.
pub fn read_json_message_sync<R: Read, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<T, IpcError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(IpcError::Io)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }

    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).map_err(IpcError::Io)?;

    serde_json::from_slice(&payload).map_err(IpcError::JsonDecode)
}

/// Write a length-prefixed JSON message over an async stream.
pub async fn write_json_message<W, T>(writer: &mut W, value: &T) -> Result<(), IpcError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value).map_err(IpcError::JsonEncode)?;
    let len = payload.len() as u32;
    writer
        .write_all(&len.to_le_bytes())
        .await
        .map_err(IpcError::Io)?;
    writer.write_all(&payload).await.map_err(IpcError::Io)?;
    Ok(())
}

/// Read a length-prefixed JSON message over an async stream. Rejects an
/// over-length declared frame before allocating.
pub async fn read_json_message<R, T>(reader: &mut R) -> Result<T, IpcError>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(IpcError::Io)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }

    let mut payload = vec![0u8; len as usize];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(IpcError::Io)?;

    serde_json::from_slice(&payload).map_err(IpcError::JsonDecode)
}

// ── Dual-read frame primitives ───────────────────────────────────────────────
//
// Read one frame's payload bytes without committing to an encoding, so a
// receiver can sniff `protocol::looks_like_json` and then decode the bytes as
// JSON (versioned envelope) or bincode (legacy). MAX_FRAME_LEN is enforced;
// first-frame requests are tiny, so the guard never trips legitimate legacy
// traffic.

/// Read one length-prefixed frame's payload bytes from an async stream.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, IpcError> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(IpcError::Io)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }
    let mut payload = vec![0u8; len as usize];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(IpcError::Io)?;
    Ok(payload)
}

/// Synchronous variant of [`read_frame`].
pub fn read_frame_sync<R: Read>(reader: &mut R) -> Result<Vec<u8>, IpcError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(IpcError::Io)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).map_err(IpcError::Io)?;
    Ok(payload)
}

/// Decode already-read frame bytes as bincode (legacy path).
pub fn decode_bincode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, IpcError> {
    bincode::deserialize(bytes).map_err(IpcError::Decode)
}

/// Decode already-read frame bytes as a JSON value (versioned path).
pub fn decode_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, IpcError> {
    serde_json::from_slice(bytes).map_err(IpcError::JsonDecode)
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io;
    use tokio::io::duplex;

    use super::*;
    use crate::types::{GrepOptions, SearchRequest, SearchResponse};

    #[tokio::test]
    async fn grep_request_round_trips() {
        let (mut client, mut server) = duplex(4096);
        let req = SearchRequest::Grep {
            query: "héllo wörld".into(),
            options: GrepOptions::default(),
        };
        write_message(&mut client, &req).await.unwrap();
        drop(client); // flush EOF so server read completes

        let rt: SearchRequest = read_message(&mut server).await.unwrap();
        match rt {
            SearchRequest::Grep { query, .. } => assert_eq!(query, "héllo wörld"),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn empty_search_results_round_trips() {
        let (mut client, mut server) = duplex(4096);
        let resp = SearchResponse::SearchResults(vec![]);
        write_message(&mut client, &resp).await.unwrap();
        drop(client);

        let rt: SearchResponse = read_message(&mut server).await.unwrap();
        match rt {
            SearchResponse::SearchResults(v) => assert!(v.is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn error_response_round_trips() {
        let (mut client, mut server) = duplex(4096);
        write_message(&mut client, &SearchResponse::Error("oops".into()))
            .await
            .unwrap();
        drop(client);

        let rt: SearchResponse = read_message(&mut server).await.unwrap();
        match rt {
            SearchResponse::Error(msg) => assert_eq!(msg, "oops"),
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn read_frame_sniffs_json_vs_bincode() {
        use crate::protocol::{RequestEnvelope, looks_like_json, verbs};
        use serde_json::json;

        // JSON envelope frame → sniff true, decodes as envelope.
        let (mut c1, mut s1) = duplex(4096);
        let env = RequestEnvelope::new(verbs::HEALTH, json!({}));
        write_json_message(&mut c1, &env).await.unwrap();
        drop(c1);
        let frame = read_frame(&mut s1).await.unwrap();
        assert!(looks_like_json(&frame));
        let back: RequestEnvelope = decode_json(&frame).unwrap();
        assert_eq!(back.verb, verbs::HEALTH);

        // Legacy bincode frame → sniff false, decodes as bincode.
        let (mut c2, mut s2) = duplex(4096);
        write_message(&mut c2, &SearchResponse::Ack).await.unwrap();
        drop(c2);
        let frame = read_frame(&mut s2).await.unwrap();
        assert!(!looks_like_json(&frame));
        let back: SearchResponse = decode_bincode(&frame).unwrap();
        assert!(matches!(back, SearchResponse::Ack));
    }

    #[tokio::test]
    async fn truncated_stream_returns_error() {
        let (mut client, mut server) = duplex(4096);
        // Write only a partial length prefix (2 bytes instead of 4)
        client.write_all(&[0u8, 1u8]).await.unwrap();
        drop(client);

        let result: Result<SearchResponse, _> = read_message(&mut server).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            IpcError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io error, got {other:?}"),
        }
    }
}
