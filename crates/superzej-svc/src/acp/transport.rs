use anyhow::Result;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use superzej_core::acp::types::JsonRpcMessage;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};

pub struct AcpReader {
    reader: FramedRead<OwnedReadHalf, LinesCodec>,
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
    writer: FramedWrite<OwnedWriteHalf, LinesCodec>,
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
    pub async fn connect(addr: &str) -> Result<(AcpReader, AcpWriter)> {
        let stream = TcpStream::connect(addr).await?;
        let (read_half, write_half) = stream.into_split();
        let reader = AcpReader {
            reader: FramedRead::new(read_half, LinesCodec::new()),
        };
        let writer = AcpWriter {
            writer: FramedWrite::new(write_half, LinesCodec::new()),
        };

        Ok((reader, writer))
    }
}
