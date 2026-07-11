//! Terminal capability detection and graceful-degradation tables.
//!
//! superzej renders its in-process chrome to the *outer* terminal. Modern
//! emulators (ghostty, wezterm, kitty, …) handle 24-bit color, full Unicode,
//! and Nerd-Font glyphs; the long tail (bare `xterm`, the Linux/BSD console,
//! Termux, Windows console, `screen`/`tmux` passthrough, CI capture, anything
//! honoring `NO_COLOR`) does not. This module turns the environment into a
//! [`TermCaps`] so the renderer can pick the richest *correct* output:
//! truecolor → 256 → 16 → monochrome for color, and Nerd-Font/Unicode → ASCII
//! for glyphs.
//!
//! Everything here is **pure** (it takes a [`TermEnv`] snapshot, never reads the
//! process environment) so it is unit-testable without a terminal — the same
//! shape as the original `undercurl_supported_env` predicate, which now lives
//! here ([`undercurl_supported_env`]). The host builds the [`TermEnv`] from
//! `std::env`, calls [`detect`], folds in config, and installs the result into
//! the render-time holders. Core carries no termwiz dependency, so [`TermCaps`]
//! is plain enums/bools — the host bridges it to termwiz colors.

/// Color fidelity the outer terminal can render, richest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit `38;2;r;g;b` direct color.
    Truecolor,
    /// 8-bit indexed (the xterm-256 palette).
    Ansi256,
    /// The 16 base ANSI colors only.
    Ansi16,
    /// No color at all (`NO_COLOR`, `TERM=dumb`): emit no SGR color.
    None,
}

/// Glyph fidelity the outer terminal + font can render, richest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnicodeLevel {
    /// UTF-8 with wide-glyph + Nerd-Font support (modern emulators).
    Full,
    /// UTF-8 but only the safe BMP set (box drawing, geometric dots).
    Basic,
    /// 7-bit ASCII only — degrade box drawing/dots/arrows to `+ - | * o ^ v`.
    Ascii,
}

/// A resolved snapshot of what the outer terminal can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermCaps {
    pub color: ColorDepth,
    pub unicode: UnicodeLevel,
    /// Curly ("undercurl") underlines (`4:3m` + `58:2::r:g:b`).
    pub undercurl: bool,
    /// Mouse reporting (SGR 1002/1006) is worth enabling.
    pub mouse: bool,
    /// OSC 52 clipboard writes are worth emitting (there is always a system
    /// clipboard fallback, so this stays on unless explicitly disabled).
    pub osc52: bool,
    /// Synchronized output (DECSET 2026) is honored — advisory.
    pub sync_output: bool,
}

impl TermCaps {
    /// The capability set for a fully modern emulator — also the value used
    /// before detection runs.
    pub const FULL: TermCaps = TermCaps {
        color: ColorDepth::Truecolor,
        unicode: UnicodeLevel::Full,
        undercurl: true,
        mouse: true,
        osc52: true,
        sync_output: true,
    };
}

impl Default for TermCaps {
    fn default() -> Self {
        TermCaps::FULL
    }
}

/// A snapshot of the terminal-relevant environment variables. The host fills
/// this from `std::env`; tests construct it directly. Empty strings and `None`
/// are treated identically (an unset / blank variable).
#[derive(Debug, Clone, Default)]
pub struct TermEnv {
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub term_program: Option<String>,
    pub vte_version: Option<String>,
    /// `true` when `NO_COLOR` is present and non-empty (per the NO_COLOR spec).
    pub no_color: bool,
    /// `WT_SESSION` (set by Windows Terminal, which is truecolor-capable).
    pub wt_session: Option<String>,
    pub lang: Option<String>,
    pub lc_all: Option<String>,
    pub lc_ctype: Option<String>,
}

impl TermEnv {
    /// Read the relevant variables from the process environment (impure).
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        TermEnv {
            term: var("TERM"),
            colorterm: var("COLORTERM"),
            term_program: var("TERM_PROGRAM"),
            vte_version: var("VTE_VERSION"),
            no_color: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            wt_session: var("WT_SESSION"),
            lang: var("LANG"),
            lc_all: var("LC_ALL"),
            lc_ctype: var("LC_CTYPE"),
        }
    }
}

/// `TERM` / `TERM_PROGRAM` substrings that identify a modern, truecolor +
/// full-Unicode + Nerd-Font emulator. Shared by color, unicode, undercurl, and
/// sync-output detection.
const MODERN_TERMS: &[&str] = &[
    "kitty",
    "wezterm",
    "foot",
    "ghostty",
    "alacritty",
    "contour",
    "rio",
    "iterm",
];

fn contains_any(hay: &str, needles: &[&str]) -> bool {
    let hay = hay.to_ascii_lowercase();
    needles.iter().any(|n| hay.contains(n))
}

/// Whether `$TERM` / `$TERM_PROGRAM` names a known-modern emulator.
fn is_modern(env: &TermEnv) -> bool {
    let term = env.term.as_deref().unwrap_or("");
    let prog = env.term_program.as_deref().unwrap_or("");
    contains_any(term, MODERN_TERMS) || contains_any(prog, MODERN_TERMS)
}

/// Whether any of `LC_ALL` / `LC_CTYPE` / `LANG` selects a UTF-8 locale.
fn locale_is_utf8(env: &TermEnv) -> bool {
    [&env.lc_all, &env.lc_ctype, &env.lang]
        .into_iter()
        .flatten()
        .any(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        })
}

/// Whether the outer terminal is known to render curly underlines, from
/// `$TERM` / `$TERM_PROGRAM` / `$VTE_VERSION`. Pure for tests. (Lives here so
/// it can be folded into [`detect`]; re-exported from the host's `wire` module
/// for backward compatibility.)
pub fn undercurl_supported_env(
    term: Option<&str>,
    term_program: Option<&str>,
    vte_version: Option<&str>,
) -> bool {
    let term = term.unwrap_or("").to_ascii_lowercase();
    let prog = term_program.unwrap_or("").to_ascii_lowercase();
    if contains_any(&term, MODERN_TERMS) || contains_any(&prog, MODERN_TERMS) {
        return true;
    }
    // VTE-based terminals support undercurl since 0.52 (VTE_VERSION=5200).
    if let Some(v) = vte_version
        && v.parse::<u32>().is_ok_and(|n| n >= 5200)
    {
        return true;
    }
    false
}

/// Resolve the terminal's color depth from the environment.
fn detect_color(env: &TermEnv) -> ColorDepth {
    if env.no_color {
        return ColorDepth::None;
    }
    let term = env.term.as_deref().unwrap_or("");
    let term_l = term.to_ascii_lowercase();
    // A dumb / unset terminal can't be assumed to handle any SGR color.
    if term_l.is_empty() || term_l == "dumb" {
        return ColorDepth::None;
    }
    // Explicit truecolor advertisement, Windows Terminal, or a known-modern
    // emulator → 24-bit.
    if let Some(ct) = env.colorterm.as_deref() {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("truecolor") || ct.contains("24bit") {
            return ColorDepth::Truecolor;
        }
    }
    if env.wt_session.is_some() || is_modern(env) {
        return ColorDepth::Truecolor;
    }
    if term_l.contains("256color") || term_l.contains("-256") {
        return ColorDepth::Ansi256;
    }
    // The Linux/BSD text console and bare xterm/vt100 get the 16 base colors.
    ColorDepth::Ansi16
}

/// Resolve the terminal's glyph level from locale + terminal identity.
fn detect_unicode(env: &TermEnv) -> UnicodeLevel {
    if !locale_is_utf8(env) {
        // A non-UTF-8 (or unset) locale can't be trusted with multibyte glyphs.
        return UnicodeLevel::Ascii;
    }
    if is_modern(env) {
        UnicodeLevel::Full
    } else {
        UnicodeLevel::Basic
    }
}

/// Build a [`TermCaps`] purely from an environment snapshot. This is the single
/// detection entry point; the host calls it with `TermEnv::from_env()` and then
/// applies any config overrides.
pub fn detect(env: &TermEnv) -> TermCaps {
    let term_l = env.term.as_deref().unwrap_or("").to_ascii_lowercase();
    let dumb = term_l.is_empty() || term_l == "dumb";
    TermCaps {
        color: detect_color(env),
        unicode: detect_unicode(env),
        undercurl: undercurl_supported_env(
            env.term.as_deref(),
            env.term_program.as_deref(),
            env.vte_version.as_deref(),
        ),
        // The Linux text console reports mouse poorly; dumb terminals not at all.
        mouse: !dumb && term_l != "linux",
        // OSC 52 always has the host-side system-clipboard fallback.
        osc52: true,
        sync_output: is_modern(env),
    }
}

/// A table of every chrome glyph that has an ASCII fallback. Selected by
/// [`UnicodeLevel`] via [`glyphs`]. All entries are `&'static str` so a holder
/// can hand out `&'static GlyphSet` with no allocation.
#[derive(Debug, Clone, Copy)]
pub struct GlyphSet {
    // Box drawing (pane frames, dividers).
    pub box_tl: &'static str,
    pub box_tr: &'static str,
    pub box_bl: &'static str,
    pub box_br: &'static str,
    pub box_h: &'static str,
    pub box_v: &'static str,
    // Status markers.
    pub dot_filled: &'static str,     // ● activity/health "on"
    pub dot_hollow: &'static str,     // ○ activity/health "idle"
    pub cross_heavy: &'static str,    // ✖ pin failed
    pub arrow_up: &'static str,       // ↑ ahead
    pub arrow_down: &'static str,     // ↓ behind
    pub diamond_filled: &'static str, // ◆ masthead
    pub diamond_hollow: &'static str, // ◇ pending step
    pub check: &'static str,          // ✓ pass
    pub cross: &'static str,          // ✗ fail
    pub ellipsis: &'static str,       // … truncation
    pub middot: &'static str,         // · separator
    pub refresh: &'static str,        // ↻ relaunch hint / active (loading) step
    pub emdash: &'static str,         // — hint separator
    pub warn: &'static str,           // ⚠ alert badge
    pub hex: &'static str,            // ⬡ open-PR badge
    pub mail: &'static str,           // ✉ unread-notification badge
    pub moon: &'static str,           // ⏾ hibernated worktree badge
    pub attention: &'static str,      // ✋ needs-you chip / blocked-on-user marker
    // Tree / sidebar chrome. POLICY: no astral-plane or emoji-presentation
    // glyphs in chrome — `Basic` terminals are BMP-only and emoji cell width
    // is font-dependent (the U+26C1 disk-badge bug class). Everything below is
    // BMP with display width 1 (asserted in tests).
    pub caret_closed: &'static str, // ▸ collapsed header
    pub caret_open: &'static str,   // ▾ expanded header
    pub tree_tee: &'static str,     // ├ tree connector (mid child)
    pub tree_corner: &'static str,  // └ tree connector (last child)
    pub half_block_r: &'static str, // ▐ sidebar cursor bar
    pub chevron: &'static str,      // › menu row lead
    pub folder: &'static str,       // ▪ sidebar folder marker
    pub dir: &'static str,          // ⌂ non-git "dir" workspace
    pub host_local: &'static str,   // ≡ local terminal / host group
    pub host_remote: &'static str,  // ⇅ remote (ssh/mosh) terminal / host group
    pub flag: &'static str,         // ⚑ merge-queue deferred / gate-failed
    pub half_dot: &'static str,     // ◐ merge-queue agent-running
    pub quote_open: &'static str,   // « env-name chip
    pub quote_close: &'static str,  // » env-name chip
    // Half-block pixel-font cells (logotype).
    pub block_full: &'static str, // █
    pub block_top: &'static str,  // ▀
    pub block_bot: &'static str,  // ▄
}

/// Full-Unicode / Nerd-Font glyphs — the current chrome look.
pub const UNICODE: GlyphSet = GlyphSet {
    box_tl: "╭",
    box_tr: "╮",
    box_bl: "╰",
    box_br: "╯",
    box_h: "─",
    box_v: "│",
    dot_filled: "\u{25cf}",     // ●
    dot_hollow: "\u{25cb}",     // ○
    cross_heavy: "\u{2716}",    // ✖
    arrow_up: "\u{2191}",       // ↑
    arrow_down: "\u{2193}",     // ↓
    diamond_filled: "\u{25c6}", // ◆
    diamond_hollow: "\u{25c7}", // ◇
    check: "\u{2713}",          // ✓
    cross: "\u{2717}",          // ✗
    ellipsis: "\u{2026}",       // …
    middot: "\u{00b7}",         // ·
    refresh: "\u{21bb}",        // ↻
    emdash: "\u{2014}",         // —
    warn: "\u{26a0}",           // ⚠
    hex: "\u{2b21}",            // ⬡
    mail: "\u{2709}",           // ✉
    moon: "\u{23fe}",           // ⏾
    attention: "\u{270b}",      // ✋ (one-line swap to `⚠` if emoji width misbehaves)
    caret_closed: "\u{25b8}",   // ▸
    caret_open: "\u{25be}",     // ▾
    tree_tee: "\u{251c}",       // ├
    tree_corner: "\u{2514}",    // └
    half_block_r: "\u{2590}",   // ▐
    chevron: "\u{203a}",        // ›
    folder: "\u{25aa}",         // ▪
    dir: "\u{2302}",            // ⌂
    host_local: "\u{2261}",     // ≡
    host_remote: "\u{21c5}",    // ⇅
    flag: "\u{2691}",           // ⚑
    half_dot: "\u{25d0}",       // ◐
    quote_open: "\u{00ab}",     // «
    quote_close: "\u{00bb}",    // »
    block_full: "\u{2588}",     // █
    block_top: "\u{2580}",      // ▀
    block_bot: "\u{2584}",      // ▄
};

/// 7-bit ASCII fallbacks for terminals/fonts that can't render [`UNICODE`].
/// Every field is plain ASCII (asserted in tests).
pub const ASCII: GlyphSet = GlyphSet {
    box_tl: "+",
    box_tr: "+",
    box_bl: "+",
    box_br: "+",
    box_h: "-",
    box_v: "|",
    dot_filled: "*",
    dot_hollow: "o",
    cross_heavy: "x",
    arrow_up: "^",
    arrow_down: "v",
    diamond_filled: "*",
    diamond_hollow: "o",
    check: "+",
    cross: "x",
    ellipsis: "...",
    middot: "-",
    refresh: "@",
    emdash: "-",
    warn: "!",
    hex: "#",
    mail: "@",
    moon: "z",
    attention: "!",
    caret_closed: ">",
    caret_open: "v",
    tree_tee: "|",
    tree_corner: "+",
    half_block_r: "|",
    chevron: ">",
    folder: "-",
    dir: "~",
    host_local: "=",
    host_remote: "@",
    flag: "!",
    half_dot: "*",
    quote_open: "<",
    quote_close: ">",
    // The pixel-font cannot render in ASCII; callers route to the text splash
    // instead, but provide safe stand-ins so a stray cell never emits a block.
    block_full: "#",
    block_top: "^",
    block_bot: "_",
};

/// The glyph table for a given level. `Full` and `Basic` share the Unicode set
/// (both are UTF-8); only `Ascii` degrades.
pub fn glyphs(level: UnicodeLevel) -> &'static GlyphSet {
    match level {
        UnicodeLevel::Full | UnicodeLevel::Basic => &UNICODE,
        UnicodeLevel::Ascii => &ASCII,
    }
}

// --- Color downsampling -------------------------------------------------------
//
// The wire renderer always composes in 24-bit truecolor; on a terminal that
// can't render it, these pure functions quantize an `(r, g, b)` triple down to
// the nearest xterm-256 index or ANSI-16 index. termwiz 0.23 ships no such
// quantizer, so we port the standard formulas here (testable without termwiz).

/// The 6 component levels of the xterm 6×6×6 color cube (indices 16..=231).
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn sq_dist(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let d = |x: u8, y: u8| {
        let v = x as i32 - y as i32;
        (v * v) as u32
    };
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

fn nearest_cube_level(v: u8) -> usize {
    let mut best = 0;
    let mut bd = u32::MAX;
    for (i, &c) in CUBE_LEVELS.iter().enumerate() {
        let d = (v as i32 - c as i32).unsigned_abs();
        if d < bd {
            bd = d;
            best = i;
        }
    }
    best
}

/// Quantize a truecolor `(r, g, b)` to the nearest xterm-256 palette index,
/// choosing whichever is closer: the 6×6×6 color cube (16..=231) or the
/// 24-step grayscale ramp (232..=255).
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    // Color-cube candidate.
    let (ri, gi, bi) = (
        nearest_cube_level(r),
        nearest_cube_level(g),
        nearest_cube_level(b),
    );
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;
    let cube_rgb = (CUBE_LEVELS[ri], CUBE_LEVELS[gi], CUBE_LEVELS[bi]);

    // Grayscale-ramp candidate: values 8, 18, … 238 at indices 232..=255.
    let gray = ((r as u32 + g as u32 + b as u32) / 3) as i32;
    let gi2 = (((gray - 8).max(0) + 5) / 10).clamp(0, 23) as u8;
    let gv = 8 + 10 * gi2;
    let gray_idx = 232 + gi2 as usize;
    let gray_rgb = (gv, gv, gv);

    let target = (r, g, b);
    if sq_dist(cube_rgb, target) <= sq_dist(gray_rgb, target) {
        cube_idx as u8
    } else {
        gray_idx as u8
    }
}

/// The canonical xterm RGB values of the 16 base ANSI colors (0..=15).
const ANSI16: [(u8, u8, u8); 16] = [
    (0, 0, 0),       // 0 black
    (205, 0, 0),     // 1 red
    (0, 205, 0),     // 2 green
    (205, 205, 0),   // 3 yellow
    (0, 0, 238),     // 4 blue
    (205, 0, 205),   // 5 magenta
    (0, 205, 205),   // 6 cyan
    (229, 229, 229), // 7 white
    (127, 127, 127), // 8 bright black
    (255, 0, 0),     // 9 bright red
    (0, 255, 0),     // 10 bright green
    (255, 255, 0),   // 11 bright yellow
    (92, 92, 255),   // 12 bright blue
    (255, 0, 255),   // 13 bright magenta
    (0, 255, 255),   // 14 bright cyan
    (255, 255, 255), // 15 bright white
];

/// The RGB value of an xterm-256 palette index: the 16 base colors, the
/// 6×6×6 cube (16..=231), and the grayscale ramp (232..=255). The inverse of
/// the cube/ramp construction in [`rgb_to_256`]; used to re-quantize a
/// 256-indexed color down to 16 colors.
pub fn index_256_to_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => ANSI16[i as usize],
        16..=231 => {
            let n = i - 16;
            let r = CUBE_LEVELS[(n / 36) as usize];
            let g = CUBE_LEVELS[((n % 36) / 6) as usize];
            let b = CUBE_LEVELS[(n % 6) as usize];
            (r, g, b)
        }
        232..=255 => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    }
}

/// Quantize a truecolor `(r, g, b)` to the nearest of the 16 base ANSI colors.
pub fn rgb_to_16(r: u8, g: u8, b: u8) -> u8 {
    let target = (r, g, b);
    let mut best = 0u8;
    let mut bd = u32::MAX;
    for (i, &c) in ANSI16.iter().enumerate() {
        let d = sq_dist(c, target);
        if d < bd {
            bd = d;
            best = i as u8;
        }
    }
    best
}

// --- Outer-terminal probe -----------------------------------------------------
//
// Env detection ([`detect`]) is authoritative and free, but it can be fooled:
// a terminal reached over `ssh`/`tmux` may carry a generic `TERM`/no
// `COLORTERM` while actually being a modern truecolor emulator. The host can
// (before it hands the tty to termwiz) write a Primary Device Attributes query
// (`CSI c`) + an XTVERSION query (`CSI > q`) and read the raw reply. termwiz
// 0.23 can't surface these responses through its input layer (they spill as
// key events — the same limit that disables the kitty keyboard protocol), so
// the host reads the raw bytes itself and hands them here. This interpreter is
// pure (no I/O); the host owns the tty-gated read.

/// What the raw probe response told us about the outer terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeResult {
    /// The terminal answered at all (a Device Attributes reply was seen).
    pub responded: bool,
    /// The terminal's self-reported name from XTVERSION (`DCS > | <name> ST`).
    pub terminal_name: Option<String>,
    /// The reported name matches a known-modern (truecolor + full-Unicode +
    /// Nerd-Font) emulator.
    pub modern: bool,
}

/// Interpret the raw bytes of a terminal's reply to `CSI c` + `CSI > q`. Looks
/// for a Primary Device Attributes reply (`CSI ? … c`) to confirm the terminal
/// responded, and an XTVERSION reply (`DCS > | <name> ST`, i.e. `ESC P > | …`)
/// to identify the emulator. Pure for tests.
pub fn interpret_probe(bytes: &[u8]) -> ProbeResult {
    let s = String::from_utf8_lossy(bytes);
    let mut r = ProbeResult::default();

    // Primary DA reply: `ESC [ ? … c`. Treat any `ESC [ ? … c` as "responded".
    if let Some(start) = s.find("\u{1b}[?")
        && s[start..].contains('c')
    {
        r.responded = true;
    }

    // XTVERSION reply: `ESC P > | <name> ESC \` (ST) — also accept a BEL
    // terminator. Capture the name between `>|` and the terminator.
    if let Some(i) = s.find(">|") {
        let rest = &s[i + 2..];
        let end = rest
            .find('\u{1b}')
            .or_else(|| rest.find('\u{07}'))
            .unwrap_or(rest.len());
        let name = rest[..end].trim().to_string();
        if !name.is_empty() {
            r.responded = true;
            r.modern = contains_any(&name, MODERN_TERMS);
            r.terminal_name = Some(name);
        }
    }
    r
}

/// Fold a probe result into env-detected capabilities. Only *upgrades* fields
/// whose config knob is `auto` (an explicit user choice always wins); never
/// downgrades. A confirmed modern terminal lifts color → truecolor, glyphs →
/// full, and enables undercurl + synchronized output.
pub fn apply_probe(
    mut caps: TermCaps,
    probe: &ProbeResult,
    color_auto: bool,
    glyph_auto: bool,
    undercurl_auto: bool,
) -> TermCaps {
    if probe.modern {
        if color_auto {
            caps.color = ColorDepth::Truecolor;
        }
        if glyph_auto {
            caps.unicode = UnicodeLevel::Full;
        }
        if undercurl_auto {
            caps.undercurl = true;
        }
        caps.sync_output = true;
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(term: &str) -> TermEnv {
        TermEnv {
            term: Some(term.into()),
            lang: Some("en_US.UTF-8".into()),
            ..Default::default()
        }
    }

    #[test]
    fn no_color_forces_mono_regardless_of_term() {
        let mut e = env("xterm-256color");
        e.colorterm = Some("truecolor".into());
        e.no_color = true;
        assert_eq!(detect(&e).color, ColorDepth::None);
    }

    #[test]
    fn dumb_and_unset_term_get_no_color() {
        assert_eq!(detect_color(&env("dumb")), ColorDepth::None);
        assert_eq!(detect_color(&TermEnv::default()), ColorDepth::None);
    }

    #[test]
    fn colorterm_truecolor_wins() {
        let mut e = env("xterm");
        e.colorterm = Some("truecolor".into());
        assert_eq!(detect_color(&e), ColorDepth::Truecolor);
        e.colorterm = Some("24bit".into());
        assert_eq!(detect_color(&e), ColorDepth::Truecolor);
    }

    #[test]
    fn windows_terminal_is_truecolor() {
        let mut e = env("xterm-256color");
        e.wt_session = Some("abc-123".into());
        assert_eq!(detect_color(&e), ColorDepth::Truecolor);
    }

    #[test]
    fn modern_emulator_is_truecolor_and_full() {
        for t in ["xterm-kitty", "wezterm", "xterm-ghostty"] {
            let c = detect(&env(t));
            assert_eq!(c.color, ColorDepth::Truecolor, "{t}");
            assert_eq!(c.unicode, UnicodeLevel::Full, "{t}");
            assert!(c.undercurl, "{t}");
            assert!(c.sync_output, "{t}");
        }
    }

    #[test]
    fn term_program_identifies_modern() {
        let mut e = env("xterm-256color");
        e.term_program = Some("iTerm.app".into());
        assert_eq!(detect_color(&e), ColorDepth::Truecolor);
        assert_eq!(detect_unicode(&e), UnicodeLevel::Full);
    }

    #[test]
    fn plain_256color_is_ansi256() {
        assert_eq!(detect_color(&env("xterm-256color")), ColorDepth::Ansi256);
        assert_eq!(detect_color(&env("screen-256color")), ColorDepth::Ansi256);
    }

    #[test]
    fn bare_term_is_ansi16() {
        assert_eq!(detect_color(&env("xterm")), ColorDepth::Ansi16);
        assert_eq!(detect_color(&env("vt100")), ColorDepth::Ansi16);
        assert_eq!(detect_color(&env("linux")), ColorDepth::Ansi16);
    }

    #[test]
    fn non_utf8_locale_forces_ascii_glyphs() {
        let e = TermEnv {
            term: Some("xterm-kitty".into()),
            lang: Some("C".into()),
            ..Default::default()
        };
        assert_eq!(detect_unicode(&e), UnicodeLevel::Ascii);
        // even a modern terminal degrades when the locale isn't UTF-8
        assert_eq!(detect(&e).unicode, UnicodeLevel::Ascii);
    }

    #[test]
    fn utf8_non_modern_is_basic() {
        assert_eq!(detect_unicode(&env("xterm-256color")), UnicodeLevel::Basic);
    }

    #[test]
    fn utf8_detected_from_any_locale_var() {
        let base = TermEnv {
            term: Some("xterm".into()),
            ..Default::default()
        };
        let mut e = base.clone();
        e.lc_all = Some("de_DE.UTF-8".into());
        assert_eq!(detect_unicode(&e), UnicodeLevel::Basic);
        let mut e = base.clone();
        e.lc_ctype = Some("ja_JP.utf8".into());
        assert_eq!(detect_unicode(&e), UnicodeLevel::Basic);
    }

    #[test]
    fn dumb_and_linux_console_disable_mouse() {
        assert!(!detect(&env("dumb")).mouse);
        assert!(!detect(&env("linux")).mouse);
        assert!(detect(&env("xterm")).mouse);
    }

    #[test]
    fn undercurl_matrix() {
        assert!(undercurl_supported_env(Some("xterm-kitty"), None, None));
        assert!(undercurl_supported_env(None, Some("WezTerm"), None));
        assert!(undercurl_supported_env(None, None, Some("6003")));
        assert!(!undercurl_supported_env(None, None, Some("5000")));
        assert!(!undercurl_supported_env(Some("xterm-256color"), None, None));
        assert!(!undercurl_supported_env(None, None, None));
    }

    #[test]
    fn osc52_stays_on() {
        // The system-clipboard fallback means OSC52 is always worth attempting.
        assert!(detect(&env("dumb")).osc52);
    }

    #[test]
    fn full_is_the_default() {
        assert_eq!(TermCaps::default(), TermCaps::FULL);
    }

    #[test]
    fn ascii_glyphs_are_all_ascii() {
        let g = glyphs(UnicodeLevel::Ascii);
        for s in [
            g.box_tl,
            g.box_tr,
            g.box_bl,
            g.box_br,
            g.box_h,
            g.box_v,
            g.dot_filled,
            g.dot_hollow,
            g.cross_heavy,
            g.arrow_up,
            g.arrow_down,
            g.diamond_filled,
            g.diamond_hollow,
            g.check,
            g.cross,
            g.ellipsis,
            g.middot,
            g.refresh,
            g.emdash,
            g.warn,
            g.hex,
            g.mail,
            g.moon,
            g.attention,
            g.caret_closed,
            g.caret_open,
            g.tree_tee,
            g.tree_corner,
            g.half_block_r,
            g.chevron,
            g.folder,
            g.dir,
            g.host_local,
            g.host_remote,
            g.flag,
            g.half_dot,
            g.quote_open,
            g.quote_close,
            g.block_full,
            g.block_top,
            g.block_bot,
        ] {
            assert!(s.is_ascii(), "non-ASCII fallback glyph: {s:?}");
            assert!(!s.is_empty(), "empty fallback glyph");
        }
    }

    #[test]
    fn unicode_glyphs_are_bmp_and_single_width() {
        // The chrome glyph policy (see the GlyphSet field docs): no astral
        // plane, no emoji-presentation width surprises. Every Unicode-table
        // glyph must be BMP and display-width 1 — the invariant that retires
        // the U+26C1 "wide checker shifts the badge" bug class. `attention`
        // (✋, U+270B) is the one sanctioned width-2 glyph: it is classified
        // East-Asian-Wide, so the seg layout already accounts for it.
        use unicode_width::UnicodeWidthStr;
        let g = glyphs(UnicodeLevel::Full);
        for s in [
            g.box_tl,
            g.box_tr,
            g.box_bl,
            g.box_br,
            g.box_h,
            g.box_v,
            g.dot_filled,
            g.dot_hollow,
            g.cross_heavy,
            g.arrow_up,
            g.arrow_down,
            g.diamond_filled,
            g.diamond_hollow,
            g.check,
            g.cross,
            g.ellipsis,
            g.middot,
            g.refresh,
            g.emdash,
            g.warn,
            g.hex,
            g.mail,
            g.moon,
            g.caret_closed,
            g.caret_open,
            g.tree_tee,
            g.tree_corner,
            g.half_block_r,
            g.chevron,
            g.folder,
            g.dir,
            g.host_local,
            g.host_remote,
            g.flag,
            g.half_dot,
            g.quote_open,
            g.quote_close,
            g.block_full,
            g.block_top,
            g.block_bot,
        ] {
            let c = s.chars().next().unwrap();
            assert!(s.chars().count() == 1, "multi-char glyph: {s:?}");
            assert!((c as u32) <= 0xFFFF, "astral-plane glyph in chrome: {s:?}");
            assert_eq!(s.width(), 1, "glyph must be display-width 1: {s:?}");
        }
        assert_eq!(g.attention.width(), 2, "✋ is the sanctioned wide glyph");
    }

    #[test]
    fn full_and_basic_share_unicode_glyphs() {
        assert_eq!(glyphs(UnicodeLevel::Full).box_tl, "╭");
        assert_eq!(glyphs(UnicodeLevel::Basic).box_tl, "╭");
        assert_eq!(glyphs(UnicodeLevel::Ascii).box_tl, "+");
    }

    #[test]
    fn rgb_to_256_maps_pure_colors() {
        // Pure black/white are the cube extremes.
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        // Pure red lands on the cube's top-red corner (16 + 36*5 = 196).
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        // Pure green/blue corners.
        assert_eq!(rgb_to_256(0, 255, 0), 46);
        assert_eq!(rgb_to_256(0, 0, 255), 21);
    }

    #[test]
    fn rgb_to_256_prefers_gray_ramp_for_grays() {
        // A mid gray is closer to the 232..255 ramp than to any cube cell.
        let idx = rgb_to_256(128, 128, 128);
        assert!((232..=255).contains(&idx), "mid gray -> ramp, got {idx}");
    }

    #[test]
    fn rgb_to_16_maps_pure_colors() {
        assert_eq!(rgb_to_16(0, 0, 0), 0);
        assert_eq!(rgb_to_16(255, 0, 0), 9); // bright red
        assert_eq!(rgb_to_16(0, 255, 0), 10); // bright green
        assert_eq!(rgb_to_16(255, 255, 255), 15); // bright white
        assert_eq!(rgb_to_16(10, 10, 10), 0); // near-black -> black
    }

    #[test]
    fn interpret_probe_reads_xtversion_and_da() {
        // ghostty: XTVERSION DCS then a DA reply.
        let bytes = b"\x1bP>|ghostty 1.0.1\x1b\\\x1b[?62;22c";
        let r = interpret_probe(bytes);
        assert!(r.responded);
        assert!(r.modern);
        assert_eq!(r.terminal_name.as_deref(), Some("ghostty 1.0.1"));
    }

    #[test]
    fn interpret_probe_da_only_responds_but_not_modern() {
        let r = interpret_probe(b"\x1b[?62;22c");
        assert!(r.responded);
        assert!(!r.modern);
        assert!(r.terminal_name.is_none());
    }

    #[test]
    fn interpret_probe_unknown_terminal_not_modern() {
        let r = interpret_probe(b"\x1bP>|someterm 0.1\x07");
        assert!(r.responded);
        assert!(!r.modern);
        assert_eq!(r.terminal_name.as_deref(), Some("someterm 0.1"));
    }

    #[test]
    fn interpret_probe_empty_means_no_response() {
        let r = interpret_probe(b"");
        assert!(!r.responded);
        assert!(!r.modern);
    }

    #[test]
    fn apply_probe_upgrades_only_auto_fields() {
        // A 16-color/ascii env baseline (e.g. ssh with generic TERM).
        let base = TermCaps {
            color: ColorDepth::Ansi16,
            unicode: UnicodeLevel::Ascii,
            undercurl: false,
            ..TermCaps::FULL
        };
        let modern = ProbeResult {
            responded: true,
            modern: true,
            terminal_name: Some("wezterm".into()),
        };
        // All auto → all upgraded.
        let up = apply_probe(base, &modern, true, true, true);
        assert_eq!(up.color, ColorDepth::Truecolor);
        assert_eq!(up.unicode, UnicodeLevel::Full);
        assert!(up.undercurl);

        // Explicit config (auto=false) is preserved despite a modern probe.
        let pinned = apply_probe(base, &modern, false, false, false);
        assert_eq!(pinned.color, ColorDepth::Ansi16);
        assert_eq!(pinned.unicode, UnicodeLevel::Ascii);
        assert!(!pinned.undercurl);

        // A non-modern probe never changes anything.
        let none = ProbeResult::default();
        assert_eq!(apply_probe(base, &none, true, true, true), base);
    }
}

// Formal proofs for the pure color-quantization math. These are compiled and run
// ONLY under `cargo kani` (the `kani` cfg + the injected `kani` crate); a normal
// `cargo build`/`cargo test`/`just ci` never sees this module, so it adds no
// dependency and no build cost. Kani solves the full 2^24 `(r, g, b)` domain
// symbolically (not by enumeration), and on every reachable path it also checks
// panic-freedom, arithmetic overflow, and out-of-bounds indexing — so the safety
// of the `CUBE_LEVELS[..]` / `ANSI16[..]` subscripts is proven implicitly. Run
// with `just verify-kani`.
#[cfg(kani)]
mod kani_proofs {
    use super::*;

    // The nearest-cube-level helper only ever returns a valid `CUBE_LEVELS`
    // index; the two public quantizers below rely on this to index safely.
    #[kani::proof]
    fn nearest_cube_level_indexes_in_bounds() {
        let v: u8 = kani::any();
        assert!(nearest_cube_level(v) < CUBE_LEVELS.len());
    }

    // Every truecolor maps to a real 256-palette color: the 6×6×6 cube band
    // (16..=231) or the grayscale ramp (232..=255), never the 0..=15 ANSI band.
    // Overflow-freedom of `16 + 36*ri + 6*gi + bi` and `8 + 10*gi2` is implicit.
    #[kani::proof]
    fn rgb_to_256_lands_in_valid_range() {
        let (r, g, b): (u8, u8, u8) = (kani::any(), kani::any(), kani::any());
        let idx = rgb_to_256(r, g, b);
        assert!((16..=231).contains(&idx) || (232..=255).contains(&idx));
    }

    // The 16-color quantizer always lands in the base ANSI band.
    #[kani::proof]
    fn rgb_to_16_in_ansi_band() {
        let (r, g, b): (u8, u8, u8) = (kani::any(), kani::any(), kani::any());
        assert!(rgb_to_16(r, g, b) <= 15);
    }

    // The inverse is total over all 256 indices — no `CUBE_LEVELS`/`ANSI16`
    // subscript is ever out of bounds (the assert just anchors the call).
    #[kani::proof]
    fn index_256_to_rgb_never_panics() {
        let i: u8 = kani::any();
        let (r, g, b) = index_256_to_rgb(i);
        let _ = (r, g, b);
    }

    // The real renderer pipeline (truecolor → 256 → re-quantize to RGB) is total
    // for every truecolor input: `rgb_to_256`'s output always feeds
    // `index_256_to_rgb` without panicking.
    #[kani::proof]
    fn index_256_of_rgb_256_is_total() {
        let (r, g, b): (u8, u8, u8) = (kani::any(), kani::any(), kani::any());
        let _ = index_256_to_rgb(rgb_to_256(r, g, b));
    }
}
