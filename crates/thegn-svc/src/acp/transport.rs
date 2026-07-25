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
    pub async fn recv(&mut self) -> Result<Option<JsonRpcMessage>> {
        if let Some(line) = self.reader.next().await {
            let line = line?;
            if line.trim().is_empty() {
                return Ok(None);
            }
            let msg: JsonRpcMessage = serde_json::from_str(&line)?;
            Ok(Some(msg))
        } else {
            Ok(None)
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

    fn frame(r: BoxRead, w: BoxWrite) -> (AcpReader, AcpWriter) {
        (
            AcpReader {
                reader: FramedRead::new(r, LinesCodec::new()),
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
    async fn blank_line_yields_none() {
        let (a, mut b) = tokio::io::duplex(64);
        let (mut reader, _) = AcpTransport::frame(Box::new(a), Box::new(tokio::io::sink()));
        b.write_all(b"\n").await.unwrap();
        assert!(reader.recv().await.unwrap().is_none());
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
