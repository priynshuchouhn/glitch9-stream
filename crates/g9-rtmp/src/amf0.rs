//! Minimal AMF0 serialization for the RTMP command messages we send
//! (`connect`, `releaseStream`, `FCPublish`, `createStream`, `publish`).
//!
//! We only need the encoder side plus enough of the decoder to read the numeric
//! stream id out of the `createStream` `_result`. Pure byte logic — tested on-host.

use bytes::{BufMut, BytesMut};

#[derive(Debug, Clone)]
#[allow(dead_code)] // Boolean/other variants are part of the complete AMF0 type set
pub enum Amf0 {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, Amf0)>),
    Null,
}

// AMF0 type markers.
const MARK_NUMBER: u8 = 0x00;
const MARK_BOOLEAN: u8 = 0x01;
const MARK_STRING: u8 = 0x02;
const MARK_OBJECT: u8 = 0x03;
const MARK_NULL: u8 = 0x05;
const MARK_OBJECT_END: u8 = 0x09;

pub fn encode(values: &[Amf0]) -> BytesMut {
    let mut b = BytesMut::new();
    for v in values {
        encode_value(&mut b, v);
    }
    b
}

fn encode_value(b: &mut BytesMut, v: &Amf0) {
    match v {
        Amf0::Number(n) => {
            b.put_u8(MARK_NUMBER);
            b.put_f64(*n);
        }
        Amf0::Boolean(x) => {
            b.put_u8(MARK_BOOLEAN);
            b.put_u8(if *x { 1 } else { 0 });
        }
        Amf0::String(s) => {
            b.put_u8(MARK_STRING);
            put_string(b, s);
        }
        Amf0::Object(fields) => {
            b.put_u8(MARK_OBJECT);
            for (k, val) in fields {
                // keys are length-prefixed UTF-8 with NO type marker
                b.put_u16(k.len() as u16);
                b.put_slice(k.as_bytes());
                encode_value(b, val);
            }
            // object end: empty key + end marker
            b.put_u16(0);
            b.put_u8(MARK_OBJECT_END);
        }
        Amf0::Null => b.put_u8(MARK_NULL),
    }
}

fn put_string(b: &mut BytesMut, s: &str) {
    b.put_u16(s.len() as u16);
    b.put_slice(s.as_bytes());
}

/// Scan an AMF0 payload for the first Number after the command name + transaction id,
/// used to read the stream id returned by `createStream` `_result`. Returns None if
/// the shape is unexpected (caller falls back to stream id 1).
pub fn find_result_stream_id(data: &[u8]) -> Option<f64> {
    let mut i = 0;
    let mut numbers = Vec::new();
    while i < data.len() {
        match data[i] {
            MARK_NUMBER if i + 9 <= data.len() => {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&data[i + 1..i + 9]);
                numbers.push(f64::from_be_bytes(arr));
                i += 9;
            }
            MARK_BOOLEAN => i += 2,
            MARK_STRING if i + 3 <= data.len() => {
                let len = u16::from_be_bytes([data[i + 1], data[i + 2]]) as usize;
                i += 3 + len;
            }
            MARK_NULL => i += 1,
            _ => i += 1, // skip objects/unknown conservatively
        }
    }
    // _result for createStream: [transactionId, (null), streamId]. The last number
    // is the stream id.
    numbers.last().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_connect_like_command() {
        let cmd = encode(&[
            Amf0::String("connect".into()),
            Amf0::Number(1.0),
            Amf0::Object(vec![
                ("app".into(), Amf0::String("live2".into())),
                ("type".into(), Amf0::String("nonprivate".into())),
            ]),
        ]);
        // first byte is string marker, then u16 len=7, "connect"
        assert_eq!(cmd[0], MARK_STRING);
        assert_eq!(&cmd[1..3], &7u16.to_be_bytes());
        assert_eq!(&cmd[3..10], b"connect");
    }

    #[test]
    fn reads_stream_id_from_result() {
        // _result, transId=2.0, null, streamId=1.0
        let payload = encode(&[
            Amf0::String("_result".into()),
            Amf0::Number(2.0),
            Amf0::Null,
            Amf0::Number(1.0),
        ]);
        assert_eq!(find_result_stream_id(&payload), Some(1.0));
    }
}
