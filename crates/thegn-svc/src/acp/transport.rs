use anyhow::Result;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use thegn_core::acp::types::JsonRpcMessage;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

// Boxed halves so the transport is stream-agnostic: TCP (non-sandboxed agent) and
// a bind-mounted unix socket (sealed sandbox — crosses the netns without network).
type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

pub struct AcpReader {
    reader: FramedRead<BoxRead, LinesCodec>,
}

impl AcpReader {
    /// Read the next framed JSON-RPC message. `Ok(None)` means genuine EOF (the
    /// peer hung up) — a *blank* line is not EOF: a peer that frames with a stray
    /// `\n\n` (or emits a keep-alive newline) must not be mistaken for a hang-up,
    /// so blank lines are skipped and we keep reading. Only a real end-of-stream
    /// or an IO/parse error terminates.
    pub async fn recv(&mut self) -> Result<Option<JsonRpcMessage>> {
        loop {
            let Some(line) = self.reader.next().await else {
                return Ok(None); // genuine EOF: stream ended
            };
            let line = line?;
            if line.trim().is_empty() {
                continue; // stray blank line — not EOF, keep reading
            }
            let msg: JsonRpcMessage = serde_json::from_str(&line)?;
            return Ok(Some(msg));
        }
    }
}

pub struct AcpWriter {
    writer: FramedWrite<BoxWrite, LinesCodec>,
}

impl AcpWriter {
    pub async fn send(&mut self, msg: &JsonRpcMessage) -> Result<()> {
        let line = serde_json::to_string(msg)?;
        self.writer.send(line).await?;
        Ok(())
    }
}

pub struct AcpTransport;

impl AcpTransport {
    /// Connect over TCP (`host:port`) — the non-sandboxed agent path.
    pub async fn connect(addr: &str) -> Result<(AcpReader, AcpWriter)> {
        let (r, w) = TcpStream::connect(addr).await?.into_split();
        Ok(Self::frame(Box::new(r), Box::new(w)))
    }

    /// Connect over a unix-domain socket — works across a sandbox netns when the
    /// socket is bind-mounted into the container (no network required).
    #[cfg(unix)]
    pub async fn connect_unix(path: &str) -> Result<(AcpReader, AcpWriter)> {
        let (r, w) = UnixStream::connect(path).await?.into_split();
        Ok(Self::frame(Box::new(r), Box::new(w)))
    }

    /// Windows stub: the socket-bind-mount path targets Linux sandboxes, which
    /// don't exist on a native Windows host. Named-pipe IPC lands separately.
    #[cfg(not(unix))]
    pub async fn connect_unix(_path: &str) -> Result<(AcpReader, AcpWriter)> {
        anyhow::bail!("ACP unix-socket transport is not supported on Windows")
    }

    /// Cap on a single JSON-RPC frame (16 MiB) — generous for any real ACP
    /// message, but bounds read-buffer growth so a peer that dumps an endless
    /// stream with no newline surfaces a codec error and closes the connection
    /// instead of ballooning host memory to OOM.
    const MAX_LINE_LEN: usize = 16 * 1024 * 1024;

    fn frame(r: BoxRead, w: BoxWrite) -> (AcpReader, AcpWriter) {
        (
            AcpReader {
                reader: FramedRead::new(r, LinesCodec::new_with_max_length(Self::MAX_LINE_LEN)),
            },
            AcpWriter {
                writer: FramedWrite::new(w, LinesCodec::new()),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::acp::types::Notification;
    use tokio::io::AsyncWriteExt;

    fn note(method: &str) -> JsonRpcMessage {
        JsonRpcMessage::Notification(Notification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params: None,
        })
    }

    /// A reader over `a` paired with a writer over `b`, where `(a, b)` are the two
    /// ends of an in-memory duplex (a real AsyncRead/AsyncWrite pair).
    fn connected() -> (AcpReader, AcpWriter) {
        let (a, b) = tokio::io::duplex(4096);
        let (reader, _) = AcpTransport::frame(Box::new(a), Box::new(tokio::io::sink()));
        let (_, writer) = AcpTransport::frame(Box::new(tokio::io::empty()), Box::new(b));
        (reader, writer)
    }

    #[tokio::test]
    async fn valid_message_round_trips() {
        let (mut reader, mut writer) = connected();
        writer.send(&note("ping")).await.unwrap();
        match reader.recv().await.unwrap().expect("a framed message") {
            JsonRpcMessage::Notification(n) => assert_eq!(n.method, "ping"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn blank_line_is_skipped_not_treated_as_eof() {
        // A stray blank line between frames (e.g. `\n\n` framing) must NOT be
        // mistaken for a hang-up: recv skips it and reads the following message.
        let (a, mut b) = tokio::io::duplex(64);
        let (mut reader, _) = AcpTransport::frame(Box::new(a), Box::new(tokio::io::sink()));
        b.write_all(b"\n").await.unwrap();
        b.write_all(serde_json::to_string(&note("ping")).unwrap().as_bytes())
            .await
            .unwrap();
        b.write_all(b"\n").await.unwrap();
        match reader
            .recv()
            .await
            .unwrap()
            .expect("blank line must be skipped")
        {
            JsonRpcMessage::Notification(n) => assert_eq!(n.method, "ping"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_line_is_an_error_not_unbounded_growth() {
        // A frame past the max length surfaces as a codec error (connection
        // closes) rather than letting the read buffer grow without bound.
        let (a, mut b) = tokio::io::duplex(64);
        let (mut reader, _) = AcpTransport::frame(Box::new(a), Box::new(tokio::io::sink()));
        let writer = tokio::spawn(async move {
            // Never send a newline; just keep pushing bytes past the cap.
            let chunk = vec![b'x'; 64 * 1024];
            loop {
                if b.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        assert!(
            reader.recv().await.is_err(),
            "an over-long unterminated line must error, not grow forever"
        );
        writer.abort();
    }

    #[tokio::test]
    async fn malformed_json_is_an_error() {
        let (a, mut b) = tokio::io::duplex(64);
        let (mut reader, _) = AcpTransport::frame(Box::new(a), Box::new(tokio::io::sink()));
        b.write_all(b"not json at all\n").await.unwrap();
        assert!(reader.recv().await.is_err());
    }

    #[tokio::test]
    async fn eof_yields_none() {
        let (a, b) = tokio::io::duplex(64);
        let (mut reader, _) = AcpTransport::frame(Box::new(a), Box::new(tokio::io::sink()));
        drop(b); // peer hang-up ⇒ stream ends
        assert!(reader.recv().await.unwrap().is_none());
    }
}
