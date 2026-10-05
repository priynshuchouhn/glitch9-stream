//! RTMP handshake (simple/plain variant, sufficient for YouTube ingest).
//!
//! C0/C1 → S0/S1/S2 → C2. We implement the simple handshake (not the
//! HMAC-digest variant); YouTube accepts it. Runs over whatever stream we're given
//! (plain TCP or a TLS stream for RTMPS).

use tokio::io::{AsyncReadExt, AsyncWriteExt};

const RTMP_VERSION: u8 = 3;
const HANDSHAKE_SIZE: usize = 1536;

/// Perform the client side of the RTMP simple handshake.
pub async fn client_handshake<S>(stream: &mut S) -> std::io::Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // --- C0 + C1 ---
    let mut c0c1 = vec![0u8; 1 + HANDSHAKE_SIZE];
    c0c1[0] = RTMP_VERSION;
    // C1: time(4) = 0, zero(4) = 0, then 1528 bytes of random.
    // bytes 1..5 time, 5..9 zero already 0.
    for (i, b) in c0c1[9..].iter_mut().enumerate() {
        *b = (i & 0xFF) as u8; // deterministic "random" is fine for the simple handshake
    }
    stream.write_all(&c0c1).await?;
    stream.flush().await?;

    // --- S0 + S1 + S2 ---
    let mut s0 = [0u8; 1];
    stream.read_exact(&mut s0).await?;
    if s0[0] != RTMP_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected RTMP server version {}", s0[0]),
        ));
    }
    let mut s1 = vec![0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut s1).await?;
    let mut s2 = vec![0u8; HANDSHAKE_SIZE];
    stream.read_exact(&mut s2).await?;

    // --- C2 (echo S1) ---
    stream.write_all(&s1).await?;
    stream.flush().await?;
    Ok(())
}
