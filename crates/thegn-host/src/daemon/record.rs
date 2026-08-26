//! The per-session recorder: tees a daemon session's PTY output into an
//! asciicast v2 `.cast` file. Owned by the [`super::session::SessionActor`] as
//! an `Option`, so recording continues while every client is detached and costs
//! a single null check per output event when off (the actor gates the tee on
//! `if let Some(rec) = …`). All the wire-format work lives in the pure
//! [`thegn_core::asciicast`] encoder; this module adds only the file sink, the
//! UTF-8 boundary carry, and the size cap.

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use thegn_core::asciicast::{CastWriter, Header, Utf8Carry};
use thegn_core::config::Config;

pub(crate) struct Recorder {
    writer: CastWriter<BufWriter<std::fs::File>>,
    path: PathBuf,
    start: Instant,
    /// `0` = uncapped.
    max_bytes: u64,
    /// Set once recording has stopped writing (size cap hit or an I/O error).
    /// After this the tee is a no-op; the file is already finalized and valid.
    done: bool,
    /// Whether `done` was reached by hitting [`Config`]`.recording.max_bytes`.
    capped: bool,
    /// Holds back a multi-byte glyph split across PTY reads (shared UTF-8 carry).
    carry: Utf8Carry,
}

impl Recorder {
    /// Start recording: create the recordings dir (0700) and the `.cast` file
    /// (0600) and write the asciicast header at the current geometry.
    pub(crate) fn start(
        session_id: &str,
        cols: u16,
        rows: u16,
        cfg: &Config,
    ) -> std::io::Result<Self> {
        let dir = cfg.recording.resolved_dir();
        std::fs::create_dir_all(&dir)?;
        crate::platform::restrict_dir_owner_only(&dir);
        let stamp = now_ms();
        // Session ids are short counters/hex, but sanitise defensively so a
        // hostile id can never escape the recordings dir.
        let safe: String = session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = dir.join(format!("{safe}_{stamp}.cast"));
        let file = crate::platform::create_private_file(&path)?;
        let mut header = Header::new(cols, rows);
        header.timestamp = Some(stamp / 1000);
        header.term = Some("thegn".to_string());
        let writer = CastWriter::new(BufWriter::new(file), &header)?;
        Ok(Recorder {
            writer,
            path,
            start: Instant::now(),
            max_bytes: cfg.recording.max_bytes,
            done: false,
            capped: false,
            carry: Utf8Carry::default(),
        })
    }

    /// Tee one raw PTY chunk. Infallible from the caller's view: an I/O error or
    /// the size cap simply stops recording (a valid file is left behind). Cheap:
    /// the only work when recording is a UTF-8 decode + one buffered write.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if self.done {
            return;
        }
        let text = self.carry.push(bytes);
        if !text.is_empty() {
            let t = self.elapsed();
            if self.writer.output(t, &text).is_err() {
                self.done = true;
                return;
            }
        }
        if self.max_bytes > 0 && self.writer.bytes_written() >= self.max_bytes {
            let _ = self.writer.flush();
            self.done = true;
            self.capped = true;
        }
    }

    /// Record a geometry change as an asciicast resize event.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        if self.done {
            return;
        }
        let t = self.elapsed();
        if self.writer.resize(t, cols, rows).is_err() {
            self.done = true;
        }
    }

    /// The `.cast` file's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written to the cast file so far.
    pub(crate) fn bytes_written(&self) -> u64 {
        self.writer.bytes_written()
    }

    /// Whether recording stopped because it hit the size cap.
    pub(crate) fn capped(&self) -> bool {
        self.capped
    }

    /// Whether recording has stopped writing (cap hit or I/O error). The actor
    /// finalizes and drops a `done` recorder.
    pub(crate) fn done(&self) -> bool {
        self.done
    }

    /// Flush the final incomplete carry and the buffer, returning the path.
    pub(crate) fn finish(mut self) -> PathBuf {
        if !self.done {
            let tail = self.carry.flush();
            if !tail.is_empty() {
                let t = self.elapsed();
                let _ = self.writer.output(t, &tail);
            }
        }
        let _ = self.writer.flush();
        self.path
    }

    fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_and_finish_write_a_parseable_cast() {
        let tmp = std::env::temp_dir().join(format!("thegn-rec-test-{}", now_ms()));
        let mut cfg = Config::default();
        cfg.recording.dir = tmp.to_string_lossy().into_owned();
        let mut rec = Recorder::start("sess-1", 80, 24, &cfg).expect("start");
        rec.feed(b"hello ");
        rec.resize(120, 40);
        rec.feed("wörld".as_bytes());
        let path = rec.finish();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 80);
        // Every remaining line is a valid event array.
        for l in lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert!(v.is_array());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn size_cap_finalizes_rather_than_growing() {
        let tmp = std::env::temp_dir().join(format!("thegn-rec-cap-{}", now_ms()));
        let mut cfg = Config::default();
        cfg.recording.dir = tmp.to_string_lossy().into_owned();
        cfg.recording.max_bytes = 200; // tiny cap
        let mut rec = Recorder::start("sess-cap", 80, 24, &cfg).expect("start");
        for _ in 0..100 {
            rec.feed(b"a lot of output to blow the cap\n");
        }
        assert!(rec.capped(), "should have hit the cap");
        assert!(rec.done());
        let before = rec.bytes_written();
        rec.feed(b"ignored after cap");
        assert_eq!(rec.bytes_written(), before, "no writes after the cap");
        let path = rec.finish();
        // The file is still a valid cast (header parses).
        let text = std::fs::read_to_string(&path).unwrap();
        let header: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(header["version"], 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
