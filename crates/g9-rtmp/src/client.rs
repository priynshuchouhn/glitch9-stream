//! RTMP(S) client: connect, publish, and send FLV-framed A/V messages.
//!
//! Supports `rtmp://` (plain TCP) and `rtmps://` (TLS via rustls). The stream key is
//! passed as the publish name and is NEVER logged. Writes use the `ChunkWriter`.
//!
//! A `Conn` abstracts over plain vs TLS streams so the handshake + chunk writes are
//! identical. On the wire this is a standard RTMP publish session that YouTube Live
//! (and Twitch/custom servers) accept.

use crate::amf0::{self, Amf0};
use crate::chunk::*;
use crate::flv;
use crate::handshake::client_handshake;
use g9_core::config::Secret;
use g9_core::frame::ParameterSets;
use g9_core::{Error, Result};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Parsed RTMP URL: scheme, host, port, app, and (for publish) the stream key.
#[derive(Debug, Clone)]
pub struct RtmpUrl {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    pub app: String,
}

impl RtmpUrl {
    /// Parse `rtmp(s)://host[:port]/app[/...]`. Any trailing path beyond the first
    /// segment is treated as part of the app for ingest endpoints like `.../live2`.
    pub fn parse(url: &str) -> Result<Self> {
        let (secure, rest) = if let Some(r) = url.strip_prefix("rtmps://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("rtmp://") {
            (false, r)
        } else {
            return Err(Error::config("rtmp url must start with rtmp:// or rtmps://"));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse().unwrap_or(if secure { 443 } else { 1935 }),
            ),
            None => (
                authority.to_string(),
                if secure { 443 } else { 1935 },
            ),
        };
        let app = path.trim_end_matches('/').to_string();
        Ok(RtmpUrl {
            secure,
            host,
            port,
            app,
        })
    }

    /// The `tcUrl` RTMP expects in `connect`: scheme://host:port/app (no key).
    pub fn tc_url(&self) -> String {
        let scheme = if self.secure { "rtmps" } else { "rtmp" };
        format!("{}://{}:{}/{}", scheme, self.host, self.port, self.app)
    }
}

/// A connected RTMP stream, generic over plain TCP or TLS.
enum Conn {
    Plain(TcpStream),
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
}

impl Conn {
    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.write_all(buf).await,
            Conn::Tls(s) => s.write_all(buf).await,
        }
    }
    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush().await,
            Conn::Tls(s) => s.flush().await,
        }
    }
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf).await,
            Conn::Tls(s) => s.read(buf).await,
        }
    }
}

pub struct RtmpClient {
    conn: Conn,
    writer: ChunkWriter,
    stream_id: u32,
    sent_video_seq: bool,
    sent_audio_seq: bool,
}

impl RtmpClient {
    /// Connect + handshake + RTMP `connect`/`publish`. The stream key is used only
    /// as the publish name; it is not logged.
    pub async fn connect_and_publish(url: &RtmpUrl, stream_key: &Secret) -> Result<Self> {
        // --- TCP ---
        let tcp = TcpStream::connect((url.host.as_str(), url.port))
            .await
            .map_err(|e| Error::transport(format!("tcp connect {}: {e}", url.host)))?;
        tcp.set_nodelay(true).ok();

        // --- Optional TLS for rtmps ---
        let mut conn = if url.secure {
            let tls = tls_connect(tcp, &url.host).await?;
            Conn::Tls(tls)
        } else {
            Conn::Plain(tcp)
        };

        // --- RTMP handshake ---
        handshake_over(&mut conn)
            .await
            .map_err(|e| Error::transport(format!("rtmp handshake: {e}")))?;

        let mut writer = ChunkWriter::new();

        // --- Set a larger chunk size (4096) to cut header overhead for video ---
        let scs = writer.encode_set_chunk_size(4096);
        conn.write_all(&scs).await.map_err(wio)?;

        // --- connect ---
        let connect = amf0::encode(&[
            Amf0::String("connect".into()),
            Amf0::Number(1.0),
            Amf0::Object(vec![
                ("app".into(), Amf0::String(url.app.clone())),
                ("type".into(), Amf0::String("nonprivate".into())),
                ("flashVer".into(), Amf0::String("FMLE/3.0 (compatible; Glitch9)".into())),
                ("tcUrl".into(), Amf0::String(url.tc_url())),
            ]),
        ]);
        let msg = writer.encode_message(CSID_COMMAND, MSG_AMF0_COMMAND, 0, 0, &connect);
        conn.write_all(&msg).await.map_err(wio)?;
        conn.flush().await.map_err(wio)?;

        // Read until we see the connect _result (and server bandwidth msgs). We do a
        // bounded read; a strict parser isn't required to proceed to publish for
        // YouTube, which accepts the pipelined commands below.
        let _ = read_some(&mut conn).await;

        // --- releaseStream + FCPublish (key as publish name) ---
        let key = stream_key.expose().to_string();
        for cmd in ["releaseStream", "FCPublish"] {
            let a = amf0::encode(&[
                Amf0::String(cmd.into()),
                Amf0::Number(0.0),
                Amf0::Null,
                Amf0::String(key.clone()),
            ]);
            let m = writer.encode_message(CSID_COMMAND, MSG_AMF0_COMMAND, 0, 0, &a);
            conn.write_all(&m).await.map_err(wio)?;
        }

        // --- createStream ---
        let cs = amf0::encode(&[
            Amf0::String("createStream".into()),
            Amf0::Number(4.0),
            Amf0::Null,
        ]);
        let m = writer.encode_message(CSID_COMMAND, MSG_AMF0_COMMAND, 0, 0, &cs);
        conn.write_all(&m).await.map_err(wio)?;
        conn.flush().await.map_err(wio)?;

        // Read the createStream _result to learn the stream id (fallback = 1).
        let resp = read_some(&mut conn).await.unwrap_or_default();
        let stream_id = amf0::find_result_stream_id(&resp).unwrap_or(1.0) as u32;

        // --- publish (key as publish name, "live") ---
        let pub_cmd = amf0::encode(&[
            Amf0::String("publish".into()),
            Amf0::Number(5.0),
            Amf0::Null,
            Amf0::String(key),
            Amf0::String("live".into()),
        ]);
        let m = writer.encode_message(CSID_COMMAND, MSG_AMF0_COMMAND, 0, stream_id, &pub_cmd);
        conn.write_all(&m).await.map_err(wio)?;
        conn.flush().await.map_err(wio)?;

        Ok(Self {
            conn,
            writer,
            stream_id,
            sent_video_seq: false,
            sent_audio_seq: false,
        })
    }

    /// Send the AVC sequence header (once per connection; call again after reconnect).
    pub async fn send_video_sequence_header(&mut self, ps: &ParameterSets) -> Result<()> {
        let payload = flv::video_sequence_header(ps);
        let m = self
            .writer
            .encode_message(CSID_VIDEO, MSG_VIDEO, 0, self.stream_id, &payload);
        self.conn.write_all(&m).await.map_err(wio)?;
        self.sent_video_seq = true;
        Ok(())
    }

    /// Send one encoded video access unit (AVCC) at `timestamp_ms`.
    pub async fn send_video(&mut self, avcc: &[u8], is_key: bool, timestamp_ms: u32) -> Result<()> {
        let payload = flv::video_nalu(avcc, is_key);
        let m = self
            .writer
            .encode_message(CSID_VIDEO, MSG_VIDEO, timestamp_ms, self.stream_id, &payload);
        self.conn.write_all(&m).await.map_err(wio)?;
        Ok(())
    }

    /// Send the AAC sequence header once (AudioSpecificConfig).
    pub async fn send_audio_sequence_header(&mut self, asc: &[u8]) -> Result<()> {
        let payload = flv::audio_sequence_header(asc);
        let m = self
            .writer
            .encode_message(CSID_AUDIO, MSG_AUDIO, 0, self.stream_id, &payload);
        self.conn.write_all(&m).await.map_err(wio)?;
        self.sent_audio_seq = true;
        Ok(())
    }

    /// Send one AAC frame at `timestamp_ms`.
    pub async fn send_audio(&mut self, aac: &[u8], timestamp_ms: u32) -> Result<()> {
        let payload = flv::audio_data(aac);
        let m = self
            .writer
            .encode_message(CSID_AUDIO, MSG_AUDIO, timestamp_ms, self.stream_id, &payload);
        self.conn.write_all(&m).await.map_err(wio)?;
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.conn.flush().await.map_err(wio)
    }

    pub fn video_seq_sent(&self) -> bool {
        self.sent_video_seq
    }
    pub fn audio_seq_sent(&self) -> bool {
        self.sent_audio_seq
    }
}

fn wio(e: std::io::Error) -> Error {
    Error::transport(format!("rtmp io: {e}"))
}

async fn read_some(conn: &mut Conn) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    match conn.read(&mut buf).await {
        Ok(n) if n > 0 => {
            buf.truncate(n);
            Some(buf)
        }
        _ => None,
    }
}

/// Run the client handshake over either stream variant.
async fn handshake_over(conn: &mut Conn) -> std::io::Result<()> {
    match conn {
        Conn::Plain(s) => client_handshake(s).await,
        Conn::Tls(s) => client_handshake(s).await,
    }
}

/// Establish a TLS session using rustls with the webpki root store.
async fn tls_connect(
    tcp: TcpStream,
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    use tokio_rustls::rustls::{self, pki_types::ServerName};
    use tokio_rustls::TlsConnector;

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| Error::transport(format!("invalid TLS server name {host}")))?;
    connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::transport(format!("tls connect: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rtmps_youtube_url() {
        let u = RtmpUrl::parse("rtmps://a.rtmps.youtube.com/live2").unwrap();
        assert!(u.secure);
        assert_eq!(u.host, "a.rtmps.youtube.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.app, "live2");
        assert_eq!(u.tc_url(), "rtmps://a.rtmps.youtube.com:443/live2");
    }

    #[test]
    fn parses_plain_rtmp_with_port() {
        let u = RtmpUrl::parse("rtmp://localhost:1935/live").unwrap();
        assert!(!u.secure);
        assert_eq!(u.port, 1935);
        assert_eq!(u.app, "live");
    }

    #[test]
    fn rejects_bad_scheme() {
        assert!(RtmpUrl::parse("http://x/y").is_err());
    }
}
