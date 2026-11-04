use serde::Serialize;
use serde_json::Value;
use std::io::{self, BufRead, Write};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// A strict, allocation-bounded NDJSON reader. A malformed/oversized frame is a
/// session error; callers must close the session instead of skipping bytes.
pub struct FrameReader<R> {
    inner: R,
}
impl<R: BufRead> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }
    pub fn read(&mut self) -> io::Result<Option<Value>> {
        let mut bytes = Vec::new();
        loop {
            let available = self.inner.fill_buf()?;
            if available.is_empty() {
                if bytes.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unterminated plugin frame",
                ));
            }
            let newline = available.iter().position(|b| *b == b'\n');
            let take = newline.map_or(available.len(), |n| n + 1);
            let needed = bytes.len().saturating_add(take);
            if needed > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized plugin frame",
                ));
            }
            if needed > bytes.capacity() {
                let target = needed
                    .max(bytes.capacity().saturating_mul(2))
                    .min(MAX_FRAME_BYTES);
                bytes.reserve_exact(target - bytes.len());
            }
            bytes.extend_from_slice(&available[..take]);
            self.inner.consume(take);
            if newline.is_some() {
                break;
            }
        }
        let value = super::canonical::decode(std::str::from_utf8(&bytes).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "plugin frame is not UTF-8")
        })?)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid canonical plugin RPC frame",
            )
        })?;
        Ok(Some(value))
    }
}

pub struct FrameWriter<W> {
    inner: W,
}
impl<W: Write> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }
    /// Validate the complete encoded message before writing any bytes. JSON
    /// escaping counts against the budget, so a rejected frame cannot corrupt
    /// the next one on the stream.
    pub fn write(&mut self, message: &impl Serialize) -> io::Result<()> {
        let mut frame = BoundedBuffer(Vec::new());
        serde_json::to_writer(&mut frame, message).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "plugin frame exceeds encoding budget",
            )
        })?;
        super::canonical::decode(
            std::str::from_utf8(&frame.0)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid encoded UTF-8"))?,
        )
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid canonical plugin RPC frame",
            )
        })?;
        self.inner.write_all(&frame.0)?;
        self.inner.write_all(b"\n")?;
        self.inner.flush()
    }
    pub fn into_inner(self) -> W {
        self.inner
    }
}
struct BoundedBuffer(Vec<u8>);
impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) >= MAX_FRAME_BYTES {
            return Err(io::Error::other("encoded plugin frame too large"));
        }
        if self.0.capacity() < self.0.len() + bytes.len() {
            let target = (self.0.len() + bytes.len())
                .max(self.0.capacity().saturating_mul(2))
                .min(MAX_FRAME_BYTES - 1);
            self.0.reserve_exact(target - self.0.len());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn framed_round_trip_and_eof() {
        let mut out = FrameWriter::new(Vec::new());
        out.write(&json!({"method":"ready","params":{"unicode":"👩🏽‍💻"}}))
            .unwrap();
        let mut input = FrameReader::new(io::Cursor::new(out.into_inner()));
        assert_eq!(input.read().unwrap().unwrap()["method"], "ready");
        assert!(input.read().unwrap().is_none());
    }
    #[test]
    fn oversized_or_unterminated_frames_fail() {
        let mut input = FrameReader::new(io::Cursor::new(vec![b' '; MAX_FRAME_BYTES + 50]));
        assert!(input.read().is_err());
        assert!(
            FrameReader::new(io::Cursor::new(b"{}".to_vec()))
                .read()
                .is_err()
        );
    }
    #[test]
    fn escaping_budget_is_checked_before_stream_write() {
        let mut out = FrameWriter::new(Vec::new());
        assert!(
            out.write(&json!({"text":"\0".repeat(MAX_FRAME_BYTES / 3)}))
                .is_err()
        );
        assert!(out.into_inner().is_empty());
    }
}

#[cfg(test)]
mod canonical_tests {
    use super::*;
    #[test]
    fn duplicate_mixed_and_noncanonical_rpc_frames_are_rejected() {
        for line in [
            "{\"method\":\"one\",\"method\":\"two\"}\n",
            "{\"method\":\"on_event\",\"params\":{\"same\":1,\"same\":2}}\n",
            "{\"method\":\"on_event\",\"result\":{}}\n",
            "{\"id\":\"1\",\"method\":\"activate\"}\n",
            "{\"id\":1,\"result\":{},\"error\":null}\n",
            "{\"id\":1,\"error\":{\"code\":\"invalid\",\"message\":\"error\",\"extra\":1}}\n",
            "{}\n",
        ] {
            assert!(
                FrameReader::new(io::Cursor::new(line)).read().is_err(),
                "accepted invalid frame"
            );
        }
    }
    #[test]
    fn depth_limit_and_partial_output_protection_apply_both_directions() {
        let mut value = serde_json::json!({});
        for _ in 0..65 {
            value = serde_json::json!({"child":value});
        }
        let frame = serde_json::json!({"method":"update","params":value});
        let mut bytes = serde_json::to_vec(&frame).unwrap();
        bytes.push(b'\n');
        assert!(FrameReader::new(io::Cursor::new(bytes)).read().is_err());
        let mut out = FrameWriter::new(Vec::new());
        assert!(out.write(&frame).is_err());
        assert!(out.into_inner().is_empty());
    }
    #[test]
    fn buffer_growth_is_capped_during_many_small_writes() {
        let mut buffer = BoundedBuffer(Vec::new());
        for _ in 0..10000 {
            buffer.write_all(b"escaped").unwrap();
            assert!(buffer.0.capacity() < MAX_FRAME_BYTES);
        }
    }
}

#[cfg(test)]
mod widget_depth_tests {
    use super::*;
    use thegn_core::plugin_app::{AppView, SplitChild, Widget};
    #[test]
    fn deepest_valid_weighted_widget_tree_survives_the_rpc_envelope() {
        let mut root = Widget::Text {
            id: "leaf".into(),
            title: String::new(),
            text: "valid".into(),
        };
        for n in 0..12 {
            root = Widget::WeightedSplit {
                id: format!("split-{n}"),
                vertical: false,
                children: vec![SplitChild {
                    widget: Box::new(root),
                    weight: 1,
                    min_size: 1,
                }],
            };
        }
        let view = AppView {
            state_scope: "test".into(),
            revision: 1,
            status: String::new(),
            root,
        };
        view.validate().unwrap();
        let mut output = FrameWriter::new(Vec::new());
        output.write(&serde_json::json!({"method":"update","params":{"surface":"test.workspace","app":view}})).unwrap();
        assert!(
            FrameReader::new(io::Cursor::new(output.into_inner()))
                .read()
                .unwrap()
                .is_some()
        );
    }
}
