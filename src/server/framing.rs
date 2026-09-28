//! WP3: length-prefixed framing for the raw-TCP game transport.
//!
//! Every frame is a `u32` big-endian payload length followed by the payload
//! (a CBOR-encoded `ClientMessage`/`ServerMessage`). The WebSocket path is
//! self-framing (WS binary frames), so only the raw-TCP game transport
//! (HTTP port + 1) and the console client use this.
//!
//! The old raw-TCP path was bare CBOR reassembly (accumulate bytes, try to
//! decode, buffer on failure) — it worked but held an unbounded-ish buffer
//! and made frame size limits ambiguous. A length prefix gives an explicit,
//! bounded frame contract for both directions.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum payload per frame. The default map's FullState is ~280 KB of
/// CBOR (200×200 grid, cities, units); handcrafted maps from `maps/` can
/// be larger still. 2 MB leaves ~7× headroom over the largest known frame
/// while still bounding the per-frame allocation a peer can demand (the
/// original 256 KB cap predates the default map's growth and silently
/// broke every raw-TCP game start: the egress loop died on the FullState
/// write and the tick loop abandoned the game within one tick).
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

/// Read exactly one frame: 4-byte BE length, then that many payload bytes.
///
/// Errors (instead of allocating) when the declared length is 0 or exceeds
/// [`MAX_FRAME_SIZE`], so a malicious or broken peer cannot trigger a huge
/// allocation — the caller must close the connection on error.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {len} out of bounds (1..={MAX_FRAME_SIZE})"),
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write exactly one frame: 4-byte BE length, then the payload.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    data: &[u8],
) -> std::io::Result<()> {
    if data.len() > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "frame length {} exceeds MAX_FRAME_SIZE {MAX_FRAME_SIZE}",
                data.len()
            ),
        ));
    }
    writer.write_all(&(data.len() as u32).to_be_bytes()).await?;
    writer.write_all(data).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_preserves_payload() {
        let (mut a, mut b) = tokio::io::duplex(8192);
        let payload: Vec<u8> = (0..1000u16).map(|i| (i % 251) as u8).collect();
        let payload_clone = payload.clone();
        tokio::spawn(async move {
            write_frame(&mut a, &payload_clone).await.unwrap();
        });
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn zero_length_header_rejected_without_allocating() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&0u32.to_be_bytes()).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn oversized_header_rejected_without_reading_payload() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // Declare MAX_FRAME_SIZE + 1, then send nothing: a correct
        // implementation must fail on the header alone, not block.
        a.write_all(&((MAX_FRAME_SIZE + 1) as u32).to_be_bytes())
            .await
            .unwrap();
        let err = tokio::time::timeout(std::time::Duration::from_secs(1), read_frame(&mut b))
            .await
            .expect("must reject on the header, not wait for a payload")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn max_size_frame_accepted() {
        let (a, mut b) = tokio::io::duplex(MAX_FRAME_SIZE + 8);
        tokio::spawn(async move {
            let mut w = a;
            // Fill exactly to the cap with a pattern (no allocation spike
            // concerns in tests; this exercises the boundary itself).
            let payload: Vec<u8> = vec![7u8; MAX_FRAME_SIZE];
            write_frame(&mut w, &payload).await.unwrap();
        });
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got.len(), MAX_FRAME_SIZE);
    }

    #[tokio::test]
    async fn write_over_max_rejected() {
        let (mut a, _b) = tokio::io::duplex(64);
        let big = vec![0u8; MAX_FRAME_SIZE + 1];
        let err = write_frame(&mut a, &big).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
