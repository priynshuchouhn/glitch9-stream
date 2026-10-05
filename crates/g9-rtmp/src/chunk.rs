//! RTMP chunk-stream writer (client → server). Enough to publish a live A/V stream.
//!
//! We write messages using a type-0 chunk header (full) for the first chunk of a
//! message and type-3 (continuation) for the rest, fragmenting the payload at the
//! negotiated chunk size. We also emit a Set Chunk Size control message so we can
//! use a large chunk size (reduces header overhead for video). Pure byte logic,
//! tested on-host; the async write happens in `transport.rs`.

use bytes::{BufMut, BytesMut};

/// RTMP message type ids we use.
pub const MSG_SET_CHUNK_SIZE: u8 = 1;
pub const MSG_AMF0_COMMAND: u8 = 20;
pub const MSG_AUDIO: u8 = 8;
pub const MSG_VIDEO: u8 = 9;
#[allow(dead_code)] // onMetaData path (not sent in the POC; kept for completeness)
pub const MSG_DATA_AMF0: u8 = 18;

/// Chunk stream ids (arbitrary but conventional).
pub const CSID_CONTROL: u32 = 2;
pub const CSID_COMMAND: u32 = 3;
pub const CSID_AUDIO: u32 = 4;
pub const CSID_VIDEO: u32 = 6;

/// Serializes RTMP messages into chunks at a given chunk size.
pub struct ChunkWriter {
    chunk_size: usize,
}

impl ChunkWriter {
    pub fn new() -> Self {
        Self { chunk_size: 128 } // RTMP default until we send Set Chunk Size
    }

    #[allow(dead_code)] // used by tests; kept as part of the ChunkWriter API
    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size;
    }

    /// Encode a "Set Chunk Size" control message and update our writer.
    pub fn encode_set_chunk_size(&mut self, size: u32) -> BytesMut {
        self.chunk_size = size as usize;
        let mut payload = BytesMut::new();
        payload.put_u32(size & 0x7FFF_FFFF);
        self.encode_message(CSID_CONTROL, MSG_SET_CHUNK_SIZE, 0, 0, &payload)
    }

    /// Encode a full RTMP message into one or more chunks.
    /// `timestamp` is the message timestamp (ms); `stream_id` is the RTMP message
    /// stream id (0 for control/command on the connection, N for media).
    pub fn encode_message(
        &self,
        csid: u32,
        msg_type: u8,
        timestamp: u32,
        stream_id: u32,
        payload: &[u8],
    ) -> BytesMut {
        let mut out = BytesMut::new();
        let len = payload.len();

        // --- First chunk: type-0 (fmt=0) basic header + message header ---
        write_basic_header(&mut out, 0, csid);
        // message header (type 0): timestamp(3) length(3) type(1) streamid(4 LE)
        let ts = if timestamp >= 0x00FF_FFFF { 0x00FF_FFFF } else { timestamp };
        out.put_u8((ts >> 16) as u8);
        out.put_u8((ts >> 8) as u8);
        out.put_u8(ts as u8);
        out.put_u8((len >> 16) as u8);
        out.put_u8((len >> 8) as u8);
        out.put_u8(len as u8);
        out.put_u8(msg_type);
        // message stream id is little-endian in RTMP
        out.put_u32_le(stream_id);
        // extended timestamp if needed
        if timestamp >= 0x00FF_FFFF {
            out.put_u32(timestamp);
        }

        // First payload fragment.
        let first = self.chunk_size.min(len);
        out.put_slice(&payload[..first]);

        // --- Continuation chunks: type-3 (fmt=3), just basic header ---
        let mut offset = first;
        while offset < len {
            write_basic_header(&mut out, 3, csid);
            if timestamp >= 0x00FF_FFFF {
                out.put_u32(timestamp); // extended ts repeats on type-3 too
            }
            let end = (offset + self.chunk_size).min(len);
            out.put_slice(&payload[offset..end]);
            offset = end;
        }
        out
    }
}

/// Write the chunk basic header for a given fmt (0..=3) and chunk stream id.
fn write_basic_header(out: &mut BytesMut, fmt: u8, csid: u32) {
    // csid 2..=63 fits in 1 byte; 64..=319 uses 2-byte form; we stay in 1-byte range.
    if csid < 64 {
        out.put_u8((fmt << 6) | (csid as u8 & 0x3F));
    } else if csid < 320 {
        out.put_u8((fmt << 6) | 0);
        out.put_u8((csid - 64) as u8);
    } else {
        out.put_u8((fmt << 6) | 1);
        let v = csid - 64;
        out.put_u8(v as u8);
        out.put_u8((v >> 8) as u8);
    }
}

impl Default for ChunkWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_chunk_small_message() {
        let w = ChunkWriter::new();
        let payload = [1u8, 2, 3, 4];
        let out = w.encode_message(CSID_COMMAND, MSG_AMF0_COMMAND, 0, 0, &payload);
        // basic header (1) + msg header (11) + payload (4)
        assert_eq!(out.len(), 1 + 11 + 4);
        assert_eq!(out[0] & 0x3F, CSID_COMMAND as u8); // csid
        assert_eq!(out[0] >> 6, 0); // fmt 0
        assert_eq!(out[7], MSG_AMF0_COMMAND); // type at offset 1+6
    }

    #[test]
    fn fragments_large_message() {
        let mut w = ChunkWriter::new();
        w.set_chunk_size(128);
        let payload = vec![0xABu8; 300]; // 128 + 128 + 44
        let out = w.encode_message(CSID_VIDEO, MSG_VIDEO, 10, 1, &payload);
        // headers: first type-0 (1+11) + two type-3 (1 each) + 300 payload bytes
        assert_eq!(out.len(), (1 + 11) + 128 + 1 + 128 + 1 + 44);
    }

    #[test]
    fn set_chunk_size_message() {
        let mut w = ChunkWriter::new();
        let out = w.encode_set_chunk_size(4096);
        assert_eq!(w.chunk_size, 4096);
        // type at offset 7 should be MSG_SET_CHUNK_SIZE
        assert_eq!(out[7], MSG_SET_CHUNK_SIZE);
    }
}
