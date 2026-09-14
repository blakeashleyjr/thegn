//! Bounded set-only OSC52 admission. Reads, empty/default selectors and clear
//! forms are deliberately unsupported; see docs/help/daemon-and-sessions.md.

pub(crate) const MAX_DECODED: usize = 64 * 1024;
pub(crate) const MAX_WIRE: usize = MAX_DECODED.div_ceil(3) * 4 + 9;

#[derive(Debug, Default)]
enum State {
    #[default]
    Text,
    Escape,
    Osc {
        escape: bool,
    },
    Discard {
        escape: bool,
    },
    SkipString {
        escape: bool,
    },
}

#[derive(Debug, Default)]
pub(crate) struct Clipboard {
    state: State,
    partial: Vec<u8>,
    pending: Option<Vec<u8>>,
    #[cfg(test)]
    scanned: usize,
}

impl Clipboard {
    pub fn reset(&mut self) {
        self.state = State::Text;
        self.partial = Vec::new();
        self.pending = None;
    }

    pub fn feed(&mut self, input: &[u8]) {
        for &byte in input {
            #[cfg(test)]
            {
                self.scanned += 1;
            }
            match self.state {
                State::Text => {
                    if byte == 0x1b {
                        self.state = State::Escape;
                    }
                }
                State::Escape => {
                    self.state = match byte {
                        b']' => {
                            self.partial.extend_from_slice(b"\x1b]");
                            State::Osc { escape: false }
                        }
                        b'P' | b'_' | b'^' | b'X' => State::SkipString { escape: false },
                        0x1b => State::Escape,
                        _ => State::Text,
                    };
                }
                State::Osc { escape } => {
                    let ended = byte == 7 || escape && byte == b'\\';
                    if self.partial.len() == MAX_WIRE {
                        self.partial = Vec::new();
                        self.state = if ended {
                            State::Text
                        } else {
                            State::Discard {
                                escape: byte == 0x1b,
                            }
                        };
                        continue;
                    }
                    // Vec's geometric capacity must obey the wire allocation
                    // cap as well as its length (MAX_WIRE is not a power of 2).
                    if self.partial.len() == self.partial.capacity() {
                        let capacity = (self.partial.capacity() * 2).max(8).min(MAX_WIRE);
                        self.partial.reserve_exact(capacity - self.partial.len());
                    }
                    self.partial.push(byte);
                    if ended {
                        let end = self.partial.len() - if byte == 7 { 1 } else { 2 };
                        if valid_set(&self.partial[2..end]) {
                            self.pending = Some(std::mem::take(&mut self.partial));
                        } else {
                            self.partial.clear();
                        }
                        self.state = State::Text;
                    } else if escape || byte.is_ascii_control() && byte != 0x1b {
                        self.partial = Vec::new();
                        self.state = State::Discard {
                            escape: byte == 0x1b,
                        };
                    } else {
                        self.state = State::Osc {
                            escape: byte == 0x1b,
                        };
                    }
                }
                State::SkipString { escape } => {
                    // DCS/APC/PM/SOS bodies may contain OSC-looking bytes or
                    // BEL. Only ST ends these strings; their contents cannot
                    // become top-level clipboard commands.
                    self.state = if escape && byte == b'\\' {
                        State::Text
                    } else {
                        State::SkipString {
                            escape: byte == 0x1b,
                        }
                    };
                }
                State::Discard { escape } => {
                    self.state = if byte == 7 || escape && byte == b'\\' {
                        State::Text
                    } else {
                        State::Discard {
                            escape: byte == 0x1b,
                        }
                    };
                }
            }
        }
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// A refused write stays owned by this pane. Rejected input cannot replace
    /// it; a later validated set can. Successful admission transfers ownership.
    pub fn submit(&mut self, admit: impl FnOnce(Vec<u8>) -> Result<(), Vec<u8>>) {
        if let Some(bytes) = self.pending.take() {
            self.pending = admit(bytes).err();
        }
    }
}

fn sextet(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Exact two-field set grammar; padding and unused low bits must be canonical.
fn valid_set(body: &[u8]) -> bool {
    let Some(rest) = body.strip_prefix(b"52;") else {
        return false;
    };
    if rest.len() < 3
        || !matches!(rest[0], b'c' | b'p' | b'q' | b's' | b'0'..=b'7')
        || rest[1] != b';'
    {
        return false;
    }
    let data = &rest[2..];
    if data.is_empty() || data.len() % 4 != 0 {
        return false;
    }
    let padding = data.iter().rev().take_while(|&&b| b == b'=').count();
    if padding > 2 {
        return false;
    }
    let decoded = data.len() / 4 * 3 - padding;
    if decoded == 0 || decoded > MAX_DECODED {
        return false;
    }
    let payload = &data[..data.len() - padding];
    if !payload.iter().all(|&b| sextet(b).is_some()) {
        return false;
    }
    let Some(last) = payload.last().and_then(|&b| sextet(b)) else {
        return false;
    };
    match padding {
        2 => last & 15 == 0,
        1 => last & 3 == 0,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending(c: &mut Clipboard) -> Option<Vec<u8>> {
        let mut result = None;
        c.submit(|bytes| {
            result = Some(bytes);
            Ok(())
        });
        result
    }
    #[test]
    fn all_selectors_and_terminators_survive_every_split() {
        for selector in b"cpqs01234567" {
            for terminator in [b"\x07".as_slice(), b"\x1b\\".as_slice()] {
                let mut wire = b"\x1b]52;".to_vec();
                wire.push(*selector);
                wire.extend_from_slice(b";eA==");
                wire.extend_from_slice(terminator);
                for split in 0..=wire.len() {
                    let mut c = Clipboard::default();
                    c.feed(&wire[..split]);
                    c.feed(&wire[split..]);
                    assert_eq!(pending(&mut c), Some(wire.clone()));
                    assert_eq!(c.scanned, wire.len());
                }
            }
        }
    }
    #[test]
    fn reads_clears_ambiguous_fields_and_noncanonical_data_never_replace_a_set() {
        for body in [
            "52;c;?",
            "52;;eA==",
            "52;c;",
            "52;cp;eA==",
            "52;x;eA==",
            "52;c;eA==;x",
            "52;c;eB==",
            "52;c;eA=",
            "52;c;====",
            "52;c;YQ==\n",
            "52;c;YQ==?",
            "52;c;YQ\x1bZ==",
        ] {
            let mut c = Clipboard::default();
            c.feed(b"\x1b]52;c;eA==\x07");
            c.submit(Err);
            c.feed(format!("\x1b]{body}\x07").as_bytes());
            assert_eq!(
                pending(&mut c).as_deref(),
                Some(b"\x1b]52;c;eA==\x07".as_slice()),
                "{body:?}"
            );
        }
    }
    #[test]
    fn valid_replacement_is_latest_and_panes_are_isolated() {
        let mut a = Clipboard::default();
        let mut b = Clipboard::default();
        a.feed(b"\x1b]52;c;eA==\x07");
        a.submit(Err);
        a.feed(b"\x1b]52;c;eQ==\x07");
        b.feed(b"\x1b]52;c;eg==\x07");
        assert_eq!(
            pending(&mut a).as_deref(),
            Some(b"\x1b]52;c;eQ==\x07".as_slice())
        );
        assert_eq!(
            pending(&mut b).as_deref(),
            Some(b"\x1b]52;c;eg==\x07".as_slice())
        );
    }
    #[test]
    fn maximum_decoded_size_and_oversize_discard_are_bounded_linear() {
        let mut wire = b"\x1b]52;c;".to_vec();
        wire.extend(vec![b'A'; MAX_DECODED / 3 * 4]);
        wire.extend_from_slice(b"AA==\x1b\\");
        let mut c = Clipboard::default();
        for byte in &wire {
            c.feed(&[*byte]);
            assert!(c.partial.capacity() <= MAX_WIRE);
        }
        assert_eq!(pending(&mut c), Some(wire.clone()));
        assert_eq!(c.scanned, wire.len());
        c.feed(b"\x1b]52;c;");
        c.feed(&vec![b'A'; MAX_WIRE * 2]);
        c.feed(b"\x1b");
        c.feed(b"\\");
        assert!(!c.has_pending());
        assert!(c.partial.is_empty());
        c.feed(b"\x1b]52;c;eA==\x07");
        assert!(c.has_pending());
    }
    #[test]
    fn clipboard_sequences_inside_other_control_strings_are_not_admitted() {
        for kind in b"P_^X" {
            let mut c = Clipboard::default();
            c.feed(&[0x1b, *kind]);
            c.feed(b"payload\x1b]52;c;eA==\x07more\x1b]52;c;eQ==\x07\x1b");
            assert!(!c.has_pending());
            c.feed(b"\\\x1b]52;c;eg==\x07");
            assert_eq!(
                pending(&mut c).as_deref(),
                Some(b"\x1b]52;c;eg==\x07".as_slice())
            );
        }
    }

    #[test]
    fn barrier_discards_both_partial_and_refused_pending() {
        let mut c = Clipboard::default();
        c.feed(b"\x1b]52;c;eA==\x07\x1b]52;c;");
        c.submit(Err);
        c.reset();
        c.feed(b"eQ==\x07");
        assert!(!c.has_pending());
        c.feed(b"\x1b]52;c;eg==\x07");
        assert!(c.has_pending());
    }
}
