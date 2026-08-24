//! "I need you" as an escape sequence: `OSC 9` and `OSC 777` parsing.
//!
//! Everything else thegn knows about an agent's state is *inferred* — CPU under
//! a worktree, unsolicited output, how long the quiet has lasted. Inference is
//! good at "working vs finished" and bad at the distinction that actually
//! matters when you are running eight agents: **finished** versus **stuck
//! waiting for you**. Both look identical from outside: no output, no CPU.
//!
//! So let the process say so. Two conventions already exist and terminals
//! already emit them:
//!
//! * `OSC 9 ; <text>` — the desktop-notification convention (iTerm2, and the
//!   shape most tools reach for first).
//! * `OSC 777 ; notify ; <title> ; <body>` — the terminal-notification
//!   convention (urxvt's `notify` module, kitty, WezTerm).
//!
//! An agent, a hook, or a bare `printf` can raise its hand with one write, and
//! the signal is authoritative rather than guessed.
//!
//! # Why a scanner and not an emulator hook
//!
//! The obvious home for this is the terminal emulator, which already parses OSC.
//! It is not available: `vte`'s `osc_dispatch` matches a closed set of OSC
//! numbers and routes everything else to a private `unhandled()` that only logs,
//! so an unrecognised OSC never reaches the `Handler` trait at all. Short of
//! forking the parser, a caller must scan the raw bytes itself — which is also
//! the better seam, because the daemon already funnels every PTY byte through
//! one function and can do this before the emulator ever sees them.
//!
//! [`OscAttentionScanner`] is therefore a *observer*, not a filter: it never
//! consumes or rewrites bytes, so the sequence still reaches the emulator and
//! the scrollback exactly as it arrived.

/// A process asking for attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionSignal {
    /// The notification title, when the convention carries one.
    pub title: Option<String>,
    /// The message body. Never empty — an empty payload is not a signal.
    pub body: String,
}

/// Longest OSC payload accumulated before giving up on a sequence. A real
/// notification is a sentence; anything longer is a runaway or a binary blob
/// that happened to contain an introducer, and must not grow the carry buffer
/// without bound.
pub const MAX_OSC_PAYLOAD: usize = 4096;

/// Parse one OSC sequence's `;`-separated parameters into an attention signal.
///
/// Returns `None` for every sequence that is not a notification — including the
/// ConEmu/Windows-Terminal `OSC 9` sub-commands (`9;4;…` progress, `9;9;…` cwd),
/// which share the number but mean something else entirely. A progress bar must
/// never read as "this agent is blocked on you".
pub fn parse_osc(params: &[&[u8]]) -> Option<AttentionSignal> {
    let (num, rest) = params.split_first()?;
    match *num {
        b"9" => {
            // Only the plain `OSC 9 ; text` form is a notification. More
            // parameters, or a lone-digit first parameter, is the ConEmu
            // extension namespace.
            let [text] = rest else { return None };
            if is_lone_digit(text) {
                return None;
            }
            signal(None, text)
        }
        b"777" => {
            let (sub, args) = rest.split_first()?;
            if *sub != b"notify" {
                return None;
            }
            match args {
                // `777;notify;<body>` — a title-only form is really a body.
                [body] => signal(None, body),
                // `777;notify;<title>;<body…>` — the body may itself contain
                // `;`, so re-join everything after the title.
                [title, body @ ..] => {
                    let joined = join(body);
                    let t = decode(title);
                    let t = (!t.is_empty()).then_some(t);
                    signal_owned(t, joined)
                }
                [] => None,
            }
        }
        _ => None,
    }
}

fn is_lone_digit(b: &[u8]) -> bool {
    b.len() == 1 && b[0].is_ascii_digit()
}

fn decode(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim().to_string()
}

fn join(parts: &[&[u8]]) -> String {
    parts
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect::<Vec<_>>()
        .join(";")
        .trim()
        .to_string()
}

fn signal(title: Option<String>, body: &[u8]) -> Option<AttentionSignal> {
    signal_owned(title, decode(body))
}

fn signal_owned(title: Option<String>, body: String) -> Option<AttentionSignal> {
    (!body.is_empty()).then_some(AttentionSignal { title, body })
}

/// Where the scanner is in a possible OSC sequence. Held across `feed` calls
/// because a PTY read can split a sequence anywhere — including between the
/// `ESC` and the `]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Not in a sequence; looking for an introducer.
    #[default]
    Idle,
    /// Saw `ESC`, waiting to see whether it introduces an OSC.
    Esc,
    /// Inside an OSC payload, accumulating until a terminator.
    Body,
    /// Inside a payload and saw `ESC`; a following `\` is the ST terminator.
    BodyEsc,
}

/// Streaming extractor for attention signals over raw PTY bytes.
///
/// Feed it every chunk; it never consumes or alters them.
#[derive(Debug, Default)]
pub struct OscAttentionScanner {
    state: State,
    payload: Vec<u8>,
    /// Set when a payload overran [`MAX_OSC_PAYLOAD`]: keep discarding until the
    /// terminator so the tail of a runaway is not parsed as a fresh sequence.
    overrun: bool,
}

impl OscAttentionScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `bytes`, appending any completed signals to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<AttentionSignal>) {
        // The overwhelmingly common case: ordinary output, mid-nothing. One
        // pass for `ESC` beats a byte-at-a-time state machine on a hot path
        // that already pays for a full emulator parse.
        if self.state == State::Idle && !bytes.contains(&0x1b) {
            return;
        }
        for &b in bytes {
            match self.state {
                State::Idle => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => match b {
                    b']' => {
                        self.state = State::Body;
                        self.payload.clear();
                        self.overrun = false;
                    }
                    // `ESC ESC` restarts the introducer rather than dropping it.
                    0x1b => {}
                    _ => self.state = State::Idle,
                },
                State::Body => match b {
                    // BEL terminates.
                    0x07 => self.finish(out),
                    // Possibly `ESC \` (ST).
                    0x1b => self.state = State::BodyEsc,
                    _ => self.push(b),
                },
                State::BodyEsc => match b {
                    b'\\' => self.finish(out),
                    // A bare ESC inside a payload abandons the sequence: the
                    // stream has moved on to something else.
                    _ => {
                        self.reset();
                        if b == 0x1b {
                            self.state = State::Esc;
                        }
                    }
                },
            }
        }
    }

    fn push(&mut self, b: u8) {
        if self.overrun {
            return;
        }
        if self.payload.len() >= MAX_OSC_PAYLOAD {
            // Stop growing, but stay in `Body` so the terminator still lands
            // here rather than leaving the scanner mid-sequence forever.
            self.overrun = true;
            self.payload.clear();
            return;
        }
        self.payload.push(b);
    }

    fn finish(&mut self, out: &mut Vec<AttentionSignal>) {
        if !self.overrun {
            let params: Vec<&[u8]> = self.payload.split(|&b| b == b';').collect();
            if let Some(sig) = parse_osc(&params) {
                out.push(sig);
            }
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.state = State::Idle;
        self.payload.clear();
        self.payload.shrink_to(MAX_OSC_PAYLOAD);
        self.overrun = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<AttentionSignal> {
        let mut s = OscAttentionScanner::new();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        out
    }

    fn one(chunk: &[u8]) -> Option<AttentionSignal> {
        scan(&[chunk]).into_iter().next()
    }

    // ── parse_osc ────────────────────────────────────────────────────────────

    #[test]
    fn osc_9_is_a_notification() {
        let sig = parse_osc(&[b"9", b"need input"]).expect("should parse");
        assert_eq!(sig.title, None);
        assert_eq!(sig.body, "need input");
    }

    #[test]
    fn osc_777_notify_carries_a_title_and_body() {
        let sig = parse_osc(&[b"777", b"notify", b"claude", b"May I proceed?"]).unwrap();
        assert_eq!(sig.title.as_deref(), Some("claude"));
        assert_eq!(sig.body, "May I proceed?");
    }

    #[test]
    fn osc_777_notify_with_one_argument_is_a_body() {
        let sig = parse_osc(&[b"777", b"notify", b"done"]).unwrap();
        assert_eq!(sig.title, None);
        assert_eq!(sig.body, "done");
    }

    #[test]
    fn a_body_containing_semicolons_is_rejoined() {
        let sig = parse_osc(&[b"777", b"notify", b"t", b"a", b"b"]).unwrap();
        assert_eq!(sig.body, "a;b");
    }

    #[test]
    fn other_777_subcommands_are_not_attention() {
        assert_eq!(parse_osc(&[b"777", b"precmd"]), None);
        assert_eq!(parse_osc(&[b"777"]), None);
        assert_eq!(parse_osc(&[b"777", b"notify"]), None);
    }

    #[test]
    fn unrelated_osc_numbers_are_ignored() {
        for num in [b"0".as_slice(), b"2", b"7", b"8", b"133", b"99"] {
            assert_eq!(parse_osc(&[num, b"whatever"]), None, "{num:?}");
        }
        assert_eq!(parse_osc(&[]), None);
    }

    /// The conflict that matters: ConEmu / Windows Terminal use `OSC 9` for
    /// progress and cwd. A progress bar must never read as "blocked on you".
    #[test]
    fn conemu_osc_9_subcommands_are_not_attention() {
        assert_eq!(parse_osc(&[b"9", b"4", b"1", b"50"]), None, "progress");
        assert_eq!(parse_osc(&[b"9", b"9", b"/home/x"]), None, "set cwd");
        assert_eq!(parse_osc(&[b"9", b"4"]), None, "lone digit");
    }

    #[test]
    fn an_empty_body_is_not_a_signal() {
        assert_eq!(parse_osc(&[b"9", b""]), None);
        assert_eq!(parse_osc(&[b"9", b"   "]), None);
        assert_eq!(parse_osc(&[b"777", b"notify", b"title", b""]), None);
    }

    #[test]
    fn a_non_utf8_body_decodes_lossily() {
        let sig = parse_osc(&[b"9", &[0xff, 0xfe, b'h', b'i']]).expect("should parse");
        assert!(sig.body.ends_with("hi"));
    }

    // ── the scanner ──────────────────────────────────────────────────────────

    #[test]
    fn a_bel_terminated_sequence_is_found() {
        let sig = one(b"working\x1b]9;need input\x07more").expect("should find");
        assert_eq!(sig.body, "need input");
    }

    #[test]
    fn an_st_terminated_sequence_is_found() {
        let sig = one(b"\x1b]777;notify;t;body\x1b\\").expect("should find");
        assert_eq!(sig.title.as_deref(), Some("t"));
        assert_eq!(sig.body, "body");
    }

    /// A PTY read splits wherever it likes — including between the `ESC` and
    /// the `]`, and mid-payload.
    #[test]
    fn a_sequence_split_across_chunks_is_reassembled() {
        assert_eq!(
            scan(&[b"\x1b", b"]9;need ", b"input", b"\x07"])
                .first()
                .map(|s| s.body.clone()),
            Some("need input".to_string())
        );
        // Split inside the ST terminator too.
        assert_eq!(
            scan(&[b"\x1b]9;hi\x1b", b"\\"])
                .first()
                .map(|s| s.body.clone()),
            Some("hi".to_string())
        );
    }

    #[test]
    fn several_signals_in_one_chunk_all_land() {
        let out = scan(&[b"\x1b]9;one\x07\x1b]9;two\x07"]);
        let bodies: Vec<&str> = out.iter().map(|s| s.body.as_str()).collect();
        assert_eq!(bodies, vec!["one", "two"]);
    }

    #[test]
    fn ordinary_output_produces_nothing() {
        assert!(scan(&[b"just some agent output\n"]).is_empty());
        // ...including other escape sequences.
        assert!(scan(&[b"\x1b[1;32mgreen\x1b[0m"]).is_empty());
        assert!(scan(&[b"\x1b]0;a window title\x07"]).is_empty());
    }

    #[test]
    fn an_unterminated_sequence_never_fires() {
        assert!(scan(&[b"\x1b]9;never ends"]).is_empty());
    }

    #[test]
    fn a_bare_esc_inside_a_payload_abandons_it() {
        // `ESC` followed by something that is not `\` is a different sequence.
        assert!(scan(&[b"\x1b]9;abc\x1b[0m\x07"]).is_empty());
    }

    #[test]
    fn a_doubled_esc_still_introduces() {
        let sig = one(b"\x1b\x1b]9;hi\x07").expect("should find");
        assert_eq!(sig.body, "hi");
    }

    /// A runaway payload must neither fire nor grow the carry buffer without
    /// bound, and the scanner must recover for the next real sequence.
    #[test]
    fn an_over_long_payload_is_abandoned_and_recovers() {
        let mut s = OscAttentionScanner::new();
        let mut out = Vec::new();
        s.feed(b"\x1b]9;", &mut out);
        for _ in 0..64 {
            s.feed(&vec![b'x'; 1024], &mut out);
            assert!(
                s.payload.len() <= MAX_OSC_PAYLOAD,
                "carry must stay bounded"
            );
        }
        s.feed(b"\x07", &mut out);
        assert!(out.is_empty(), "a runaway must not fire");
        s.feed(b"\x1b]9;recovered\x07", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].body, "recovered");
    }

    #[test]
    fn the_scanner_is_an_observer_not_a_filter() {
        // `feed` takes a shared slice, so it cannot rewrite the stream; this
        // test documents the contract that the emulator still sees everything.
        let bytes = b"\x1b]9;hi\x07rest".to_vec();
        let before = bytes.clone();
        let mut s = OscAttentionScanner::new();
        let mut out = Vec::new();
        s.feed(&bytes, &mut out);
        assert_eq!(bytes, before);
        assert_eq!(out.len(), 1);
    }
}
