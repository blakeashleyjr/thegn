//! Bounded, pollable log following. File identity survives rename, unlike a
//! pathname. Initial history is skipped; replacement generations start at zero.
//! Copytruncate followed by regrowth past our cursor between polls is inherently
//! indistinguishable from append and is not promised lossless delivery here.

use same_file::Handle;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

const READ_BUDGET: usize = 64 * 1024;
const LINE_LIMIT: usize = 1024 * 1024;

struct Active {
    handle: Handle,
    offset: u64,
    partial: Vec<u8>,
    truncated: bool,
    discard_prefix: bool,
}

impl Active {
    fn open(mut handle: Handle, skip_history: bool) -> std::io::Result<Self> {
        let offset = if skip_history {
            handle.as_file_mut().seek(SeekFrom::End(0))?
        } else {
            0
        };
        let discard_prefix = if skip_history && offset > 0 {
            handle.as_file_mut().seek(SeekFrom::Start(offset - 1))?;
            let mut last = [0];
            handle.as_file_mut().read_exact(&mut last)?;
            last[0] != b'\n'
        } else {
            false
        };
        Ok(Self {
            handle,
            offset,
            partial: Vec::new(),
            truncated: false,
            discard_prefix,
        })
    }

    fn read(&mut self, budget: usize, lines: &mut Vec<String>) -> std::io::Result<usize> {
        if self.handle.as_file().metadata()?.len() < self.offset {
            self.handle.as_file_mut().seek(SeekFrom::Start(0))?;
            self.offset = 0;
            self.partial.clear();
            self.truncated = false;
            self.discard_prefix = false;
        }
        let mut bytes = Vec::new();
        let n = self
            .handle
            .as_file_mut()
            .take(budget as u64)
            .read_to_end(&mut bytes)?;
        self.offset += n as u64;
        for byte in bytes {
            if self.discard_prefix {
                if byte == b'\n' {
                    self.discard_prefix = false;
                }
                continue;
            }
            if byte == b'\n' {
                if self.partial.last() == Some(&b'\r') {
                    self.partial.pop();
                }
                let mut line = String::from_utf8_lossy(&self.partial).into_owned();
                if self.truncated {
                    line.push_str(" [log record truncated at 1 MiB]");
                }
                lines.push(line);
                self.partial.clear();
                self.truncated = false;
            } else if self.partial.len() < LINE_LIMIT {
                self.partial.push(byte);
            } else {
                self.truncated = true;
            }
        }
        Ok(n)
    }
}

pub(super) struct FileFollower {
    path: PathBuf,
    active: Option<Active>,
    first_poll: bool,
    more: bool,
}

impl FileFollower {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            active: None,
            first_poll: true,
            more: false,
        }
    }

    /// Read at most READ_BUDGET bytes, including any retired generation. On
    /// replacement, retire the old descriptor after this one bounded drain even
    /// if its writer is still appending; otherwise the new file could starve.
    /// A partial retired record is dropped, never joined to replacement bytes.
    pub(super) fn poll(&mut self) -> Vec<String> {
        let skip_history = std::mem::take(&mut self.first_poll);
        let mut lines = Vec::new();
        let mut remaining = READ_BUDGET;
        self.more = false;
        // best-effort: a temporarily missing/unreadable path is retried next poll
        if let Ok(handle) = std::fs::File::open(&self.path).and_then(Handle::from_file) {
            let replaced = self
                .active
                .as_ref()
                .is_some_and(|active| active.handle != handle);
            if replaced {
                if let Some(active) = &mut self.active {
                    // best-effort: a retired descriptor error must not strand its replacement
                    remaining -= active.read(remaining, &mut lines).unwrap_or(0);
                }
                self.active = None;
            }
            if self.active.is_none() {
                // best-effort: failed attach is retried; replacement history starts at zero
                match Active::open(handle, skip_history) {
                    Ok(active) => self.active = Some(active),
                    Err(_) => return lines,
                }
            }
        }
        if let Some(active) = &mut self.active {
            // best-effort: transient reads are retried without terminating the follower
            self.more = active
                .read(remaining, &mut lines)
                .is_ok_and(|n| n == remaining);
        }
        lines
    }

    pub(super) fn has_more(&self) -> bool {
        self.more
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    fn append(path: &std::path::Path, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, FileFollower) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active.log");
        fs::write(&path, b"historical\n").unwrap();
        let mut reader = FileFollower::new(path.clone());
        assert!(reader.poll().is_empty());
        (dir, path, reader)
    }

    #[test]
    fn skips_history_and_waits_for_a_complete_record() {
        let (_dir, path, mut reader) = fixture();
        append(&path, b"hel");
        assert!(reader.poll().is_empty());
        append(&path, b"lo\r\nnext\n");
        assert_eq!(reader.poll(), ["hello", "next"]);
        assert!(reader.poll().is_empty());
    }

    #[test]
    fn rotation_drains_old_complete_lines_then_reads_new_from_zero() {
        let (dir, path, mut reader) = fixture();
        append(&path, b"old complete\nold partial");
        fs::rename(&path, dir.path().join("retired.log")).unwrap();
        fs::write(&path, b"replacement\n").unwrap();
        assert_eq!(reader.poll(), ["old complete", "replacement"]);
        assert!(reader.poll().is_empty());
    }

    #[test]
    fn missing_path_and_recreation_keep_the_follower_alive() {
        let (dir, path, mut reader) = fixture();
        let retired = dir.path().join("retired.log");
        fs::rename(&path, &retired).unwrap();
        append(&retired, b"old tail\n");
        assert_eq!(reader.poll(), ["old tail"]);
        assert!(reader.poll().is_empty());
        fs::write(&path, b"new head\n").unwrap();
        assert_eq!(reader.poll(), ["new head"]);
    }

    #[test]
    fn initially_absent_file_is_read_when_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("later.log");
        let mut reader = FileFollower::new(path.clone());
        assert!(reader.poll().is_empty());
        fs::write(&path, b"first\n").unwrap();
        assert_eq!(reader.poll(), ["first"]);
    }

    #[test]
    fn initial_attachment_does_not_emit_the_suffix_of_historical_partial_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active.log");
        fs::write(&path, b"old incomplete").unwrap();
        let mut reader = FileFollower::new(path.clone());
        assert!(reader.poll().is_empty());
        append(&path, b" suffix\nnew line\n");
        assert_eq!(reader.poll(), ["new line"]);
    }

    #[test]
    fn observed_truncation_resets_cursor_and_partial_record() {
        let (_dir, path, mut reader) = fixture();
        append(&path, b"unfinished previous generation");
        assert!(reader.poll().is_empty());
        fs::write(&path, b"new\n").unwrap();
        assert_eq!(reader.poll(), ["new"]);
    }

    #[test]
    fn retired_writer_cannot_starve_replacement() {
        let (dir, path, mut reader) = fixture();
        let retired = dir.path().join("retired.log");
        fs::rename(&path, &retired).unwrap();
        append(&retired, &vec![b'x'; READ_BUDGET * 2]);
        fs::write(&path, b"replacement\n").unwrap();
        assert!(reader.poll().is_empty()); // at most one budget spent on retired
        append(&retired, &vec![b'y'; READ_BUDGET]);
        assert_eq!(reader.poll(), ["replacement"]);
    }

    #[test]
    fn oversized_partial_records_have_bounded_memory() {
        let (_dir, path, mut reader) = fixture();
        append(&path, &vec![b'x'; LINE_LIMIT + READ_BUDGET]);
        for _ in 0..(LINE_LIMIT / READ_BUDGET + 1) {
            assert!(reader.poll().is_empty());
            assert!(reader.active.as_ref().unwrap().partial.len() <= LINE_LIMIT);
        }
        append(&path, b"\nnext\n");
        let lines = reader.poll();
        assert!(lines[0].ends_with(" [log record truncated at 1 MiB]"));
        assert_eq!(lines[1], "next");
    }
}
