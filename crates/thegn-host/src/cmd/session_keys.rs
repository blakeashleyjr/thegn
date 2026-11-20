use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use regex::Regex;
use termwiz::input::{KeyCode, Modifiers};

/// One key and its modifiers, parsed from `--key` / MCP `keys` (see D3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeySpec {
    pub(crate) code: KeyCode,
    pub(crate) mods: Modifiers,
}

pub(crate) const MAX_KEYS: usize = 64;

pub(crate) fn parse_key(spec: &str) -> Result<KeySpec> {
    let mut rest = spec;
    let mut mods = Modifiers::NONE;

    loop {
        let lower = rest.to_ascii_lowercase();
        let modifier = [
            ("ctrl+", Modifiers::CTRL),
            ("alt+", Modifiers::ALT),
            ("shift+", Modifiers::SHIFT),
            ("c-", Modifiers::CTRL),
            ("m-", Modifiers::ALT),
            ("s-", Modifiers::SHIFT),
        ]
        .into_iter()
        .find(|(prefix, _)| lower.starts_with(prefix));
        let Some((prefix, modifier)) = modifier else {
            break;
        };
        mods |= modifier;
        rest = &rest[prefix.len()..];
    }

    let lower = rest.to_ascii_lowercase();
    let mut chars = rest.chars();
    let code = match (chars.next(), chars.next()) {
        // One character is sent as itself (case kept): `f`, `G`, `:`.
        (Some(c), None) => KeyCode::Char(c),
        _ => match lower.as_str() {
            "up" => KeyCode::UpArrow,
            "down" => KeyCode::DownArrow,
            "left" => KeyCode::LeftArrow,
            "right" => KeyCode::RightArrow,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pgup" => KeyCode::PageUp,
            "pgdn" => KeyCode::PageDown,
            "insert" => KeyCode::Insert,
            "delete" => KeyCode::Delete,
            "esc" => KeyCode::Escape,
            "enter" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "space" => KeyCode::Char(' '),
            name => match name
                .strip_prefix('f')
                .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|n| n.parse::<u8>().ok())
            {
                Some(n @ 1..=12) => KeyCode::Function(n),
                _ => bail!(
                    "unknown key name (expected up, down, left, right, home, end, pgup, pgdn, insert, delete, esc, enter, tab, backspace, space, f1-f12 or one character)"
                ),
            },
        },
    };

    Ok(KeySpec { code, mods })
}

/// The bytes the pane expects, through the TUI's own encoder.
pub(crate) fn encode_keys(keys: &[KeySpec], app_cursor: bool) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for key in keys {
        let encoded = crate::input::key_bytes_mode(&key.code, key.mods, app_cursor)
            .ok_or_else(|| anyhow::anyhow!("cannot encode key {:?}", key.code))?;
        bytes.extend_from_slice(&encoded);
    }
    Ok(bytes)
}

/// `text` wrapped in ESC[200~ … ESC[201~ when the pane enabled bracketed paste;
/// refuses text that contains ESC[201~ (it would end the paste early).
pub(crate) fn paste_bytes(text: &str, bracketed: bool) -> Result<Vec<u8>> {
    if text.contains("\x1b[201~") {
        bail!("paste text contains the bracketed-paste end marker")
    }
    if bracketed {
        let mut bytes = Vec::with_capacity(text.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        Ok(bytes)
    } else {
        Ok(text.as_bytes().to_vec())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScreenModes {
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
    pub(crate) alt_screen: bool,
    pub(crate) app_cursor: bool,
    pub(crate) bracketed_paste: bool,
    pub(crate) mouse: &'static str,
    pub(crate) sgr_mouse: bool,
    pub(crate) title: Option<String>,
}

fn strip_title_controls(title: String) -> String {
    title.chars().filter(|c| !c.is_control()).collect()
}

/// Replay the snapshot's ANSI repaint into an emulator and read back its modes.
pub(crate) fn screen_modes(rows: u16, cols: u16, ansi: &[u8]) -> ScreenModes {
    use crate::emulator::{MouseMode, PaneEmulator};

    let mut emulator = crate::emulator::AlacrittyEmulator::new(rows.max(1), cols.max(1), 0);
    emulator.advance(ansi);
    let (cursor_row, cursor_col) = emulator.cursor();
    let (mouse_mode, sgr_mouse) = emulator.mouse_mode();
    let mouse = match mouse_mode {
        MouseMode::None => "none",
        MouseMode::Press => "press",
        MouseMode::PressRelease => "press_release",
        MouseMode::ButtonMotion => "button_motion",
        MouseMode::AnyMotion => "any_motion",
    };
    ScreenModes {
        cursor_row,
        cursor_col,
        alt_screen: emulator.alt_screen(),
        app_cursor: emulator.application_cursor(),
        bracketed_paste: emulator.bracketed_paste(),
        mouse,
        sgr_mouse,
        title: emulator.title().map(strip_title_controls),
    }
}

pub(crate) enum ScreenWait {
    Matches(Regex),
    Stable(Duration),
}

/// `screen:<regex>` / `stable:<ms>`; `Ok(None)` for every other condition.
pub(crate) fn parse_screen_wait(condition: &str) -> Result<Option<ScreenWait>> {
    if let Some(rx) = condition.strip_prefix("screen:") {
        let regex = regex::RegexBuilder::new(rx).size_limit(1 << 20).build()?;
        return Ok(Some(ScreenWait::Matches(regex)));
    }
    if let Some(ms) = condition.strip_prefix("stable:") {
        let millis = ms.parse::<u64>()?;
        return Ok(Some(ScreenWait::Stable(Duration::from_millis(millis))));
    }
    Ok(None)
}

struct StableClock {
    seq: Option<u64>,
    since: Instant,
}

impl StableClock {
    fn observe(&mut self, seq: u64, now: Instant, need: Duration) -> bool {
        if self.seq != Some(seq) {
            self.seq = Some(seq);
            self.since = now;
            return need.is_zero();
        }
        now.duration_since(self.since) >= need
    }
}

/// Poll `client.snapshot` every 100 ms until the condition holds or the timeout passes.
pub(crate) async fn wait_screen(
    client: &thegn_svc::control::client::ControlClient,
    session: &str,
    wait: &ScreenWait,
    timeout_ms: Option<i64>,
) -> Result<serde_json::Value> {
    let started = Instant::now();
    let timeout = timeout_ms
        .filter(|ms| *ms >= 0)
        .map(|ms| Duration::from_millis(ms.unsigned_abs()));
    let condition = match wait {
        ScreenWait::Matches(regex) => format!("screen:{}", regex.as_str()),
        ScreenWait::Stable(duration) => format!("stable:{}", duration.as_millis()),
    };
    let mut stable = StableClock {
        seq: None,
        since: started,
    };
    let mut last_seq: Option<u64> = None;

    loop {
        let (seq, rows, cols, ansi) = client.snapshot(session).await?;
        let matched = match wait {
            ScreenWait::Matches(regex) => {
                let text = super::snapshot_text(rows, cols, &ansi);
                regex.is_match(&text)
            }
            ScreenWait::Stable(need) => stable.observe(seq, Instant::now(), *need),
        };
        if matched {
            return Ok(serde_json::json!({
                "matched": true,
                "condition": condition,
                "exit_code": null,
            }));
        }
        if last_seq == Some(seq) {
            // A dead session's screen never changes (the daemon serves its tombstone);
            // answer as the daemon's own wait does for a corpse.
            let probe = client
                .wait(session, serde_json::json!({ "kind": "exited" }), Some(1))
                .await?;
            if probe.get("matched").and_then(serde_json::Value::as_bool) == Some(true) {
                return Ok(probe);
            }
        }
        last_seq = Some(seq);
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            return Ok(serde_json::json!({
                "matched": false,
                "condition": condition,
                "exit_code": null,
            }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::term_snapshot::{ScreenSnapshot, SnapCell, encode_ansi};

    #[test]
    fn parse_key_table() {
        let cases = [
            ("up", KeyCode::UpArrow, Modifiers::NONE),
            ("ESC", KeyCode::Escape, Modifiers::NONE),
            ("ctrl+c", KeyCode::Char('c'), Modifiers::CTRL),
            ("C-c", KeyCode::Char('c'), Modifiers::CTRL),
            ("alt+x", KeyCode::Char('x'), Modifiers::ALT),
            ("M-x", KeyCode::Char('x'), Modifiers::ALT),
            ("shift+tab", KeyCode::Tab, Modifiers::SHIFT),
            ("f12", KeyCode::Function(12), Modifiers::NONE),
            ("f", KeyCode::Char('f'), Modifiers::NONE),
            ("G", KeyCode::Char('G'), Modifiers::NONE),
            ("space", KeyCode::Char(' '), Modifiers::NONE),
            ("a", KeyCode::Char('a'), Modifiers::NONE),
        ];
        for (input, code, mods) in cases {
            assert_eq!(parse_key(input).unwrap(), KeySpec { code, mods });
        }
        for input in ["f13", "f+5", "ctrl+", "", "hyper+x"] {
            assert!(parse_key(input).is_err(), "{input} should be rejected");
        }
    }

    #[test]
    fn encode_honours_decckm() {
        assert_eq!(
            encode_keys(&[parse_key("up").unwrap()], false).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(
            encode_keys(&[parse_key("up").unwrap()], true).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            encode_keys(&[parse_key("ctrl+c").unwrap()], false).unwrap(),
            [0x03]
        );
        assert_eq!(
            encode_keys(&[parse_key("shift+tab").unwrap()], false).unwrap(),
            b"\x1b[Z"
        );
        assert_eq!(
            encode_keys(&[parse_key("f5").unwrap()], false).unwrap(),
            b"\x1b[15~"
        );
    }

    #[test]
    fn paste_wraps_only_when_bracketed() {
        assert_eq!(paste_bytes("hello", false).unwrap(), b"hello");
        assert_eq!(
            paste_bytes("hello", true).unwrap(),
            b"\x1b[200~hello\x1b[201~"
        );
    }

    #[test]
    fn paste_refuses_an_embedded_end_marker() {
        assert!(paste_bytes("before\x1b[201~after", false).is_err());
        assert!(paste_bytes("before\x1b[201~after", true).is_err());
    }

    #[test]
    fn screen_modes_reads_the_repaint() {
        let rows = 8;
        let cols = 20;
        let snapshot = ScreenSnapshot {
            rows,
            cols,
            cursor: (3, 5),
            cursor_visible: true,
            alt_screen: true,
            app_cursor: true,
            bracketed_paste: true,
            mouse: 1002,
            sgr_mouse: true,
            title: "test title".into(),
            history_tail: String::new(),
            cells: vec![SnapCell::default(); rows as usize * cols as usize],
            seq: 0,
        };
        let modes = screen_modes(rows, cols, &encode_ansi(&snapshot));
        assert_eq!(
            modes,
            ScreenModes {
                cursor_row: 3,
                cursor_col: 5,
                alt_screen: true,
                app_cursor: true,
                bracketed_paste: true,
                mouse: "button_motion",
                sgr_mouse: true,
                title: Some("test title".into()),
            }
        );
    }

    #[test]
    fn parse_screen_wait_forms() {
        assert!(matches!(
            parse_screen_wait("screen:a+").unwrap(),
            Some(ScreenWait::Matches(_))
        ));
        assert!(matches!(
            parse_screen_wait("stable:500").unwrap(),
            Some(ScreenWait::Stable(duration)) if duration == Duration::from_millis(500)
        ));
        assert!(parse_screen_wait("exited").unwrap().is_none());
        assert!(parse_screen_wait("stable:x").is_err());
    }

    #[test]
    fn stable_clock_needs_an_unchanged_seq_for_the_whole_window() {
        let start = Instant::now();
        let mut clock = StableClock {
            seq: None,
            since: start,
        };
        let need = Duration::from_millis(500);
        assert!(!clock.observe(1, start, need));
        assert!(!clock.observe(1, start + Duration::from_millis(499), need));
        assert!(!clock.observe(2, start + Duration::from_millis(500), need));
        assert!(!clock.observe(2, start + Duration::from_millis(999), need));
        assert!(clock.observe(2, start + Duration::from_millis(1_000), need));
    }
}
