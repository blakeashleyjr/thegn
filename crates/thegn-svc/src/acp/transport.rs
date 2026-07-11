use anyhow::Result;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use thegn_core::acp::types::JsonRpcMessage;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
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
    pub async fn connect_unix(path: &str) -> Result<(AcpReader, AcpWriter)> {
        let (r, w) = UnixStream::connect(path).await?.into_split();
        Ok(Self::frame(Box::new(r), Box::new(w)))
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
