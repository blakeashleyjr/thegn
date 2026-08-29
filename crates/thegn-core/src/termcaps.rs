//! Terminal capability detection and graceful-degradation tables.
//!
//! thegn renders its in-process chrome to the *outer* terminal. Modern
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
    /// `LC_TERMINAL` — iTerm2 and WezTerm both set it, and unlike `TERM_PROGRAM`
    /// it **survives ssh**: it matches the `SendEnv LC_*` pattern that every
    /// stock `ssh_config` already forwards.
    ///
    /// That is exactly the case `probe.rs` exists to rescue — "a terminal
    /// reached over ssh/tmux carrying a generic `TERM`" — and this answers it
    /// from the environment, with no I/O and no 80ms probe budget.
    pub lc_terminal: Option<String>,
    /// `TERM_PROGRAM_VERSION` — a terminal's own build number. Only meaningful
    /// alongside `term_program`, and (like it) does NOT survive ssh; there is no
    /// `LC_` twin. Used for version-gated capabilities, the way `vte_version`
    /// already gates undercurl.
    pub term_program_version: Option<String>,
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
            lc_terminal: var("LC_TERMINAL"),
            term_program_version: var("TERM_PROGRAM_VERSION"),
            vte_version: var("VTE_VERSION"),
            no_color: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            wt_session: var("WT_SESSION"),
            lang: var("LANG"),
            lc_all: var("LC_ALL"),
            lc_ctype: var("LC_CTYPE"),
        }
    }

    /// The emulator's self-reported name: `TERM_PROGRAM`, or `LC_TERMINAL` when
    /// that is unset. Over ssh only the latter survives, so a terminal that
    /// identifies itself locally must not become anonymous one hop away.
    pub fn program_name(&self) -> Option<&str> {
        self.term_program
            .as_deref()
            .or(self.lc_terminal.as_deref())
            .filter(|s| !s.is_empty())
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

/// Whether `$TERM` / `$TERM_PROGRAM` / `$LC_TERMINAL` names a known-modern
/// emulator.
fn is_modern(env: &TermEnv) -> bool {
    let term = env.term.as_deref().unwrap_or("");
    let prog = env.program_name().unwrap_or("");
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

/// Terminal.app builds at or above this `TERM_PROGRAM_VERSION` render 24-bit
/// color. Below it, 256 colors is the honest answer and must stay the default —
/// every Terminal.app before macOS 26 genuinely had no truecolor.
///
/// **470 because 470.2 is the build that was actually looked at.** The floor is a
/// claim about *rendering*, which a version string cannot establish — it was set
/// by displaying a 24-bit gradient plus a 24-step grey ramp one unit apart in
/// Terminal.app 470.2 on macOS 26.5.1 and confirming the ramp is smooth rather
/// than the ~3 flat blocks a 256-colour quantizer produces.
///
/// Deliberately *not* lowered to guess at the first truecolor build: some
/// macOS 15 Terminal.app may also qualify, but nobody has looked, and the cost of
/// guessing low is banded colour on a terminal we promised truecolor to. Guessing
/// high only costs a terminal the 256-colour output it already had. Lower it when
/// someone verifies an earlier build the same way.
const APPLE_TERMINAL_TRUECOLOR_FLOOR: Option<u32> = Some(470);

/// Whether this is a Terminal.app new enough to render 24-bit color.
///
/// Deliberately consulted by [`detect_color`] **only**, never folded into
/// [`is_modern`]: that would also flip glyphs to `Full`, `sync_output` to true
/// and undercurl to true, none of which Terminal.app has. Colour is the one
/// capability that changed.
///
/// An absent, unparseable, or below-floor version keeps today's answer, so the
/// gate can only ever upgrade a terminal we positively identified.
fn apple_terminal_truecolor(term_program: Option<&str>, version: Option<&str>) -> bool {
    let Some(floor) = APPLE_TERMINAL_TRUECOLOR_FLOOR else {
        return false;
    };
    if !term_program
        .unwrap_or("")
        .eq_ignore_ascii_case("apple_terminal")
    {
        return false;
    }
    // The version is a bare build number ("455", "470.2") — compare the major.
    version
        .and_then(|v| v.split('.').next())
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= floor)
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
    // Terminal.app: colour-only, version-gated. See the constant for why the
    // gate is currently inert.
    if apple_terminal_truecolor(
        env.term_program.as_deref(),
        env.term_program_version.as_deref(),
    ) {
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
    // Windows Terminal renders full Unicode natively but sets no POSIX locale
    // vars — don't let the locale check demote it to ASCII.
    if env.wt_session.is_some() {
        return UnicodeLevel::Full;
    }
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
        // Windows Terminal renders undercurl + DECSET 2026 synchronized
        // output (both since WT 1.18) but isn't named by $TERM/$TERM_PROGRAM.
        undercurl: undercurl_supported_env(
            env.term.as_deref(),
            env.program_name(),
            env.vte_version.as_deref(),
        ) || env.wt_session.is_some(),
        // The Linux text console reports mouse poorly; dumb terminals not at all.
        mouse: !dumb && term_l != "linux",
        // OSC 52 always has the host-side system-clipboard fallback.
        osc52: true,
        sync_output: is_modern(env) || env.wt_session.is_some(),
    }
}

/// Whether the environment shows evidence of a modern terminal host: Windows
/// Terminal, a known-modern emulator, an explicit truecolor advertisement, or
/// at least a 256-color `$TERM`. The Windows host refuses to start the
/// compositor without it — legacy conhost.exe renders VT sequences too poorly
/// to degrade gracefully, and looking broken is worse than a clear error.
pub fn modern_terminal_evidence(env: &TermEnv) -> bool {
    if env.wt_session.is_some() || is_modern(env) {
        return true;
    }
    if let Some(ct) = env.colorterm.as_deref() {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("truecolor") || ct.contains("24bit") {
            return true;
        }
    }
    let term_l = env.term.as_deref().unwrap_or("").to_ascii_lowercase();
    term_l.contains("256color") || term_l.contains("-256")
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
    pub arrow_right: &'static str,    // → flows-into / next-stage marker
    pub diamond_filled: &'static str, // ◆ generic emphasis marker
    pub diamond_hollow: &'static str, // ◇ pending step
    pub role_server: &'static str,    // ▲ daemon serving remote thin clients
    pub role_client: &'static str,    // ▽ attached to a remote daemon
    pub brand_sigil: &'static str,    // þ masthead brand mark (OE thorn, "þegn")
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
    pub jj: &'static str,             // ĵ jujutsu-colocated worktree marker
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
    pub gauge: &'static str,        // ◔ AI-account usage badge
    pub quote_open: &'static str,   // « env-name chip
    pub quote_close: &'static str,  // » env-name chip
    // Weather condition classes (`crate::weather::Sky`) — one glyph per class,
    // not per provider code, so "light rain shower" and "patchy rain" draw the
    // same mark. Same BMP, width-1 policy as the rest of the chrome: the
    // obvious picks (⛅ U+26C5, ⚡ U+26A1) are Emoji-Presentation and therefore
    // width 2 — do not reach for them.
    pub wx_clear: &'static str,  // ☀
    pub wx_partly: &'static str, // ☼
    pub wx_cloudy: &'static str, // ☁
    pub wx_fog: &'static str,    // ≈
    pub wx_rain: &'static str,   // ☂
    pub wx_snow: &'static str,   // ☃
    pub wx_storm: &'static str,  // ☇
    pub wx_wind: &'static str,   // ↝
    // Half-block pixel-font cells (logotype).
    pub block_full: &'static str, // █
    pub block_top: &'static str,  // ▀
    pub block_bot: &'static str,  // ▄
    // Loading-splash liveness (spinner frames + progress bar). The spinner is
    // the quarter-circle family (same East-Asian-Ambiguous exposure as the
    // shipped `half_dot` ◐, BMP width-1) so `Basic` terminals render it too.
    pub spin: &'static [&'static str], // ◐◓◑◒ animated active-step frames
    pub bar_fill: &'static str,        // ▓ progress-bar filled cell
    pub bar_empty: &'static str,       // ░ progress-bar empty cell
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
    arrow_right: "\u{2192}",    // →
    diamond_filled: "\u{25c6}", // ◆
    diamond_hollow: "\u{25c7}", // ◇
    role_server: "\u{25b2}",    // ▲
    role_client: "\u{25bd}",    // ▽
    brand_sigil: "\u{00fe}",    // þ — Latin-1, width 1, safe at Full AND Basic
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
    jj: "\u{0135}",             // ĵ (Latin j-with-circumflex: BMP, width-1)
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
    // Same block and the same East-Asian-Ambiguous exposure as `half_dot`, so a
    // terminal that renders ◐ at width 1 renders this one too.
    gauge: "\u{25d4}",                                       // ◔
    quote_open: "\u{00ab}",                                  // «
    quote_close: "\u{00bb}",                                 // »
    wx_clear: "\u{2600}",                                    // ☀
    wx_partly: "\u{263c}",                                   // ☼
    wx_cloudy: "\u{2601}",                                   // ☁
    wx_fog: "\u{2248}",                                      // ≈
    wx_rain: "\u{2602}",                                     // ☂
    wx_snow: "\u{2603}",                                     // ☃
    wx_storm: "\u{2607}",                                    // ☇ (literally "thunderstorm")
    wx_wind: "\u{219d}",                                     // ↝
    block_full: "\u{2588}",                                  // █
    block_top: "\u{2580}",                                   // ▀
    block_bot: "\u{2584}",                                   // ▄
    spin: &["\u{25d0}", "\u{25d3}", "\u{25d1}", "\u{25d2}"], // ◐ ◓ ◑ ◒
    bar_fill: "\u{2593}",                                    // ▓
    bar_empty: "\u{2591}",                                   // ░
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
    arrow_right: ">",
    diamond_filled: "*",
    diamond_hollow: "o",
    role_server: "^",
    role_client: "v",
    brand_sigil: "*",
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
    jj: "j",
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
    gauge: "%",
    quote_open: "<",
    quote_close: ">",
    wx_clear: "*",
    wx_partly: "*",
    wx_cloudy: "=",
    wx_fog: "~",
    wx_rain: "'",
    wx_snow: "#",
    wx_storm: "!",
    wx_wind: "~",
    // The pixel-font cannot render in ASCII; callers route to the text splash
    // instead, but provide safe stand-ins so a stray cell never emits a block.
    block_full: "#",
    block_top: "^",
    block_bot: "_",
    spin: &["|", "/", "-", "\\"],
    bar_fill: "=",
    bar_empty: "-",
};

/// The glyph table for a given level. `Full` and `Basic` share the Unicode set
/// (both are UTF-8); only `Ascii` degrades.
pub fn glyphs(level: UnicodeLevel) -> &'static GlyphSet {
    match level {
        UnicodeLevel::Full | UnicodeLevel::Basic => &UNICODE,
        UnicodeLevel::Ascii => &ASCII,
    }
}

/// A resolvable glyph token — the missing half of the token vocabulary.
///
/// Colors already have [`crate::theme`]'s slot/hue tokens, resolved once per
/// line against the live palette; glyphs had only bare [`GlyphSet`] field reads
/// scattered across draw sites (the glyph-literal ratchet debt). This enum names
/// each chrome glyph as a token so element content — and any draw site — can
/// carry `Glyph::DotFilled` instead of a raw `"●"`, degrading through the one
/// chokepoint (`caps::active_glyphs()`) exactly as colors quantize once in
/// `wire.rs`. Pure data (no host dependency, core-coverage-gated); the host's
/// `caps::glyph` resolves a token against the active set.
///
/// Every variant maps to one single-cell [`GlyphSet`] field. The animated
/// `spin` frames (a `&[&str]`, not one glyph) are deliberately excluded — a
/// spinner is a frame index, not a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Glyph {
    BoxTl,
    BoxTr,
    BoxBl,
    BoxBr,
    BoxH,
    BoxV,
    DotFilled,
    DotHollow,
    CrossHeavy,
    ArrowUp,
    ArrowDown,
    ArrowRight,
    DiamondFilled,
    DiamondHollow,
    RoleServer,
    RoleClient,
    BrandSigil,
    Check,
    Cross,
    Ellipsis,
    Middot,
    Refresh,
    Emdash,
    Warn,
    Hex,
    Mail,
    Moon,
    Attention,
    CaretClosed,
    CaretOpen,
    TreeTee,
    TreeCorner,
    HalfBlockR,
    Chevron,
    Folder,
    Dir,
    HostLocal,
    HostRemote,
    Flag,
    HalfDot,
    Gauge,
    QuoteOpen,
    QuoteClose,
    WxClear,
    WxPartly,
    WxCloudy,
    WxFog,
    WxRain,
    WxSnow,
    WxStorm,
    WxWind,
    BlockFull,
    BlockTop,
    BlockBot,
    BarFill,
    BarEmpty,
}

impl Glyph {
    /// Every token — the exhaustive list, so a consumer (and the mapping test)
    /// can iterate the whole vocabulary. A new variant not added here fails the
    /// `every_glyph_token_resolves` test.
    pub const ALL: &'static [Glyph] = &[
        Glyph::BoxTl,
        Glyph::BoxTr,
        Glyph::BoxBl,
        Glyph::BoxBr,
        Glyph::BoxH,
        Glyph::BoxV,
        Glyph::DotFilled,
        Glyph::DotHollow,
        Glyph::CrossHeavy,
        Glyph::ArrowUp,
        Glyph::ArrowDown,
        Glyph::ArrowRight,
        Glyph::DiamondFilled,
        Glyph::DiamondHollow,
        Glyph::RoleServer,
        Glyph::RoleClient,
        Glyph::BrandSigil,
        Glyph::Check,
        Glyph::Cross,
        Glyph::Ellipsis,
        Glyph::Middot,
        Glyph::Refresh,
        Glyph::Emdash,
        Glyph::Warn,
        Glyph::Hex,
        Glyph::Mail,
        Glyph::Moon,
        Glyph::Attention,
        Glyph::CaretClosed,
        Glyph::CaretOpen,
        Glyph::TreeTee,
        Glyph::TreeCorner,
        Glyph::HalfBlockR,
        Glyph::Chevron,
        Glyph::Folder,
        Glyph::Dir,
        Glyph::HostLocal,
        Glyph::HostRemote,
        Glyph::Flag,
        Glyph::HalfDot,
        Glyph::Gauge,
        Glyph::QuoteOpen,
        Glyph::QuoteClose,
        Glyph::WxClear,
        Glyph::WxPartly,
        Glyph::WxCloudy,
        Glyph::WxFog,
        Glyph::WxRain,
        Glyph::WxSnow,
        Glyph::WxStorm,
        Glyph::WxWind,
        Glyph::BlockFull,
        Glyph::BlockTop,
        Glyph::BlockBot,
        Glyph::BarFill,
        Glyph::BarEmpty,
    ];

    /// Resolve this token to its glyph in `set` — the single-cell `&'static str`
    /// for the active capability level. Total over every variant; the caller
    /// (`caps::glyph`) passes `active_glyphs()`, so the ASCII fallback is
    /// selected at the chokepoint with no branching at the draw site.
    pub fn resolve(self, set: &GlyphSet) -> &'static str {
        match self {
            Glyph::BoxTl => set.box_tl,
            Glyph::BoxTr => set.box_tr,
            Glyph::BoxBl => set.box_bl,
            Glyph::BoxBr => set.box_br,
            Glyph::BoxH => set.box_h,
            Glyph::BoxV => set.box_v,
            Glyph::DotFilled => set.dot_filled,
            Glyph::DotHollow => set.dot_hollow,
            Glyph::CrossHeavy => set.cross_heavy,
            Glyph::ArrowUp => set.arrow_up,
            Glyph::ArrowDown => set.arrow_down,
            Glyph::ArrowRight => set.arrow_right,
            Glyph::DiamondFilled => set.diamond_filled,
            Glyph::DiamondHollow => set.diamond_hollow,
            Glyph::RoleServer => set.role_server,
            Glyph::RoleClient => set.role_client,
            Glyph::BrandSigil => set.brand_sigil,
            Glyph::Check => set.check,
            Glyph::Cross => set.cross,
            Glyph::Ellipsis => set.ellipsis,
            Glyph::Middot => set.middot,
            Glyph::Refresh => set.refresh,
            Glyph::Emdash => set.emdash,
            Glyph::Warn => set.warn,
            Glyph::Hex => set.hex,
            Glyph::Mail => set.mail,
            Glyph::Moon => set.moon,
            Glyph::Attention => set.attention,
            Glyph::CaretClosed => set.caret_closed,
            Glyph::CaretOpen => set.caret_open,
            Glyph::TreeTee => set.tree_tee,
            Glyph::TreeCorner => set.tree_corner,
            Glyph::HalfBlockR => set.half_block_r,
            Glyph::Chevron => set.chevron,
            Glyph::Folder => set.folder,
            Glyph::Dir => set.dir,
            Glyph::HostLocal => set.host_local,
            Glyph::HostRemote => set.host_remote,
            Glyph::Flag => set.flag,
            Glyph::HalfDot => set.half_dot,
            Glyph::Gauge => set.gauge,
            Glyph::QuoteOpen => set.quote_open,
            Glyph::QuoteClose => set.quote_close,
            Glyph::WxClear => set.wx_clear,
            Glyph::WxPartly => set.wx_partly,
            Glyph::WxCloudy => set.wx_cloudy,
            Glyph::WxFog => set.wx_fog,
            Glyph::WxRain => set.wx_rain,
            Glyph::WxSnow => set.wx_snow,
            Glyph::WxStorm => set.wx_storm,
            Glyph::WxWind => set.wx_wind,
            Glyph::BlockFull => set.block_full,
            Glyph::BlockTop => set.block_top,
            Glyph::BlockBot => set.block_bot,
            Glyph::BarFill => set.bar_fill,
            Glyph::BarEmpty => set.bar_empty,
        }
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
    /// xterm `modifyOtherKeys` level reported by XTQMODKEYS
    /// (`CSI > 4 ; <Pv> m`). `None` = the terminal did not answer the query.
    pub modify_other_keys: Option<u8>,
    /// kitty keyboard-protocol flags reported by the progressive-enhancement
    /// query (`CSI ? <flags> u`). `None` = the terminal did not answer.
    pub kitty_keyboard: Option<u8>,
}

/// The keyboard-reporting queries the startup probe writes, in order, BEFORE
/// XTVERSION + Primary DA (the DA reply is the batch terminator).
///
/// `CSI ? u`   — kitty progressive-enhancement query.
/// `CSI ? 4 m` — XTQMODKEYS (xterm modifyOtherKeys level).
/// `CSI m`     — plain SGR reset. Mandatory insurance: `CSI ? 4 m` carries a
///               private-parameter marker that a conformant parser ignores,
///               but a sloppy one could read as `SGR 4` (underline).
pub const KEYBOARD_QUERIES: &[u8] = b"\x1b[?u\x1b[?4m\x1b[m";

impl ProbeResult {
    /// Whether the terminal can report `Ctrl+<digit>` / `Ctrl+Alt+<digit>`
    /// distinctly from a legacy control byte.
    ///
    /// `Some(true)`  — confirmed: `modifyOtherKeys` is at level >= 2, which is
    ///                 what thegn's chord matching needs (termwiz pushes level
    ///                 2 in `set_raw_mode`, and the probe runs after that push,
    ///                 so this reads the level actually in effect).
    /// `Some(false)` — confirmed broken: either XTQMODKEYS answered with a
    ///                 level below 2, or the terminal answered the kitty query
    ///                 but not XTQMODKEYS (a kitty-protocol-only terminal,
    ///                 where thegn's `modifyOtherKeys` push provably did
    ///                 nothing — thegn does not push the kitty protocol).
    /// `None`        — cannot tell (no probe, or the terminal was silent on
    ///                 both queries). Callers MUST treat this as "assume it
    ///                 works": an unknown never suppresses an affordance.
    pub fn ctrl_digit_reportable(&self) -> Option<bool> {
        match (self.modify_other_keys, self.kitty_keyboard) {
            (Some(level), _) => Some(level >= 2),
            (None, Some(_)) => Some(false),
            (None, None) => None,
        }
    }
}

/// Find a CSI reply of the shape `<prefix><digits and ';'><final>` anywhere in
/// `s` and return the parameter text between the prefix and the final byte.
/// The final byte is what disambiguates otherwise-identical prefixes — a
/// Primary DA reply and a kitty keyboard reply both start `ESC [ ?` and differ
/// only in their `c` / `u` terminator. A sequence cut short by a truncated read
/// simply doesn't match, so a partial buffer degrades to "unknown".
fn csi_reply<'a>(s: &'a str, prefix: &str, final_byte: char) -> Option<&'a str> {
    let mut from = 0;
    while let Some(off) = s[from..].find(prefix) {
        let params_at = from + off + prefix.len();
        let rest = &s[params_at..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != ';')
            .unwrap_or(rest.len());
        if rest[end..].starts_with(final_byte) {
            return Some(&rest[..end]);
        }
        from = from + off + prefix.len();
    }
    None
}

/// Whether `bytes` already contains a complete Primary Device Attributes reply
/// (`ESC [ ? <digits and ';'> c`).
///
/// The probe asks for DA **last**, so this is the signal that every earlier
/// reply has arrived and the read can stop. It must be a *strict* match: since
/// [`KEYBOARD_QUERIES`] the buffer also holds a kitty reply (`ESC [ ? … u`),
/// which shares the `ESC [ ?` prefix, and an XTVERSION name is arbitrary text
/// that routinely contains a `c` (`Alacritty`, `contour` — both in
/// [`MODERN_TERMS`]). A loose "a `?` somewhere, then any `c`" rule therefore
/// ends the read on the *version* reply and leaves the DA bytes in the tty for
/// the input reader to decode as stray keystrokes. See THE-70.
pub fn has_primary_da(bytes: &[u8]) -> bool {
    csi_reply(&String::from_utf8_lossy(bytes), "\u{1b}[?", 'c').is_some()
}

/// Interpret the raw bytes of a terminal's reply to [`KEYBOARD_QUERIES`] +
/// `CSI > q` + `CSI c`. Looks for a Primary Device Attributes reply
/// (`CSI ? … c`) to confirm the terminal responded, an XTVERSION reply
/// (`DCS > | <name> ST`, i.e. `ESC P > | …`) to identify the emulator, and the
/// two keyboard-reporting replies (XTQMODKEYS `CSI > 4 ; <Pv> m` and the kitty
/// progressive-enhancement `CSI ? <flags> u`). The replies may arrive in any
/// order and interleaved; each is searched for independently, and any of them
/// counts as "the terminal responded". Pure for tests.
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

    // XTQMODKEYS reply: `ESC [ > 4 ; <Pv> m`. A value we can't parse (garbage
    // or out of range) stays `None` — "the terminal didn't tell us" is always
    // safer than a wrong level.
    if let Some(params) = csi_reply(&s, "\u{1b}[>4;", 'm') {
        r.responded = true;
        r.modify_other_keys = params.parse::<u8>().ok(); // best-effort: optional input: an unparseable reply means 'not reported'
    }

    // kitty progressive-enhancement reply: `ESC [ ? <flags> u`. Same prefix as
    // the Primary DA reply above — the terminator (`u` vs `c`) is what tells
    // them apart, which `csi_reply` keys on.
    if let Some(params) = csi_reply(&s, "\u{1b}[?", 'u') {
        r.responded = true;
        r.kitty_keyboard = params.parse::<u8>().ok(); // best-effort: optional input: an unparseable reply means 'not reported'
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
    fn windows_terminal_gets_full_unicode_without_posix_locale() {
        // WT sets no LANG/LC_*: the locale check must not demote it to ASCII.
        let e = TermEnv {
            wt_session: Some("abc-123".into()),
            ..Default::default()
        };
        let caps = detect(&e);
        assert_eq!(caps.unicode, UnicodeLevel::Full);
        assert!(caps.undercurl, "WT ≥1.18 renders undercurl");
        assert!(caps.sync_output, "WT ≥1.18 supports DECSET 2026");
    }

    #[test]
    fn apple_terminal_gate_upgrades_colour_only_and_never_below_the_floor() {
        // The gate is colour-only by construction. Even with the floor active,
        // Terminal.app must NOT become "modern": it has no wide-glyph story, no
        // undercurl and no synchronised output, and routing it through
        // `is_modern` would silently claim all three.
        let mut e = TermEnv {
            term: Some("xterm-256color".into()),
            term_program: Some("Apple_Terminal".into()),
            term_program_version: Some("470.2".into()),
            lang: Some("en_US.UTF-8".into()),
            ..Default::default()
        };
        let caps = detect(&e);
        assert!(!caps.undercurl, "Terminal.app has no undercurl");
        assert!(!caps.sync_output, "Terminal.app has no DECSET 2026");
        assert_eq!(
            caps.unicode,
            UnicodeLevel::Basic,
            "Terminal.app is not a wide-glyph terminal"
        );

        // The pure gate itself, independent of the (currently inert) floor.
        // Only an Apple_Terminal with a parseable major at-or-above the floor
        // qualifies — everything else keeps today's answer, so the gate can only
        // upgrade a terminal we positively identified.
        assert!(!apple_terminal_truecolor(Some("iTerm.app"), Some("999")));
        assert!(!apple_terminal_truecolor(Some("Apple_Terminal"), None));
        assert!(!apple_terminal_truecolor(
            Some("Apple_Terminal"),
            Some("not-a-number")
        ));
        assert!(!apple_terminal_truecolor(None, Some("470.2")));

        // While the floor is unset the gate is off, and Terminal.app keeps the
        // conservative 256-colour answer that is correct for every build before
        // macOS 26. This assertion is what turns "enable the gate" into a
        // deliberate act rather than a silent one.
        match APPLE_TERMINAL_TRUECOLOR_FLOOR {
            None => assert_eq!(detect(&e).color, ColorDepth::Ansi256),
            Some(floor) => {
                e.term_program_version = Some(format!("{}", floor.saturating_sub(1)));
                assert_eq!(
                    detect(&e).color,
                    ColorDepth::Ansi256,
                    "a below-floor build must stay 256-colour"
                );
                e.term_program_version = Some(format!("{floor}"));
                assert_eq!(detect(&e).color, ColorDepth::Truecolor);
            }
        }
    }

    #[test]
    fn lc_terminal_identifies_the_emulator_when_term_program_is_gone() {
        // The ssh case. `TERM_PROGRAM` is NOT forwarded by ssh; `LC_TERMINAL`
        // (set by iTerm2 and WezTerm) is, because every stock `ssh_config`
        // already carries `SendEnv LC_*`. Without reading it, a truecolor
        // emulator one hop away is indistinguishable from a dumb 256-color
        // terminal — the exact case the 80ms DA/XTVERSION probe exists to
        // rescue, answered here for free.
        // `LANG` rides the same `SendEnv` as `LC_TERMINAL`, so a real ssh
        // session has both; `detect_unicode` needs the UTF-8 locale.
        let sshed = TermEnv {
            term: Some("xterm-256color".into()),
            lc_terminal: Some("iTerm2".into()),
            lang: Some("en_US.UTF-8".into()),
            ..Default::default()
        };
        let caps = detect(&sshed);
        assert_eq!(caps.color, ColorDepth::Truecolor);
        assert_eq!(caps.unicode, UnicodeLevel::Full);
        assert!(caps.undercurl);
        assert!(caps.sync_output);

        // Same shape without it stays conservatively degraded — proving the
        // upgrade came from `LC_TERMINAL` and nothing else.
        let anon = TermEnv {
            term: Some("xterm-256color".into()),
            lang: Some("en_US.UTF-8".into()),
            ..Default::default()
        };
        assert_eq!(detect(&anon).color, ColorDepth::Ansi256);
        assert!(!detect(&anon).undercurl);

        // `TERM_PROGRAM` still wins when both are present (it is the local,
        // first-hand answer); `LC_TERMINAL` is only the fallback.
        let both = TermEnv {
            term_program: Some("Apple_Terminal".into()),
            lc_terminal: Some("iTerm2".into()),
            ..Default::default()
        };
        assert_eq!(both.program_name(), Some("Apple_Terminal"));
        assert!(!detect(&both).undercurl);
    }

    #[test]
    fn modern_terminal_evidence_gates_conhost() {
        // Bare conhost: no WT_SESSION, no TERM/COLORTERM — refused.
        assert!(!modern_terminal_evidence(&TermEnv::default()));
        // Windows Terminal.
        let wt = TermEnv {
            wt_session: Some("s".into()),
            ..Default::default()
        };
        assert!(modern_terminal_evidence(&wt));
        // Known-modern emulator name, truecolor advert, or a 256-color TERM.
        assert!(modern_terminal_evidence(&env("wezterm")));
        let ct = TermEnv {
            colorterm: Some("truecolor".into()),
            ..Default::default()
        };
        assert!(modern_terminal_evidence(&ct));
        assert!(modern_terminal_evidence(&env("xterm-256color")));
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
    fn brand_sigil_is_single_cell_in_both_sets() {
        // Masthead layout assumes a 1-col sigil. þ (U+00FE) is Latin-1 and
        // display-width 1 everywhere; keep it a single narrow scalar.
        assert_eq!(UNICODE.brand_sigil, "\u{00fe}");
        assert_eq!(UNICODE.brand_sigil.chars().count(), 1);
        assert_eq!(ASCII.brand_sigil.chars().count(), 1);
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
            g.arrow_right,
            g.diamond_filled,
            g.diamond_hollow,
            g.role_server,
            g.role_client,
            g.brand_sigil,
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
            g.jj,
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
            g.gauge,
            g.quote_open,
            g.quote_close,
            g.wx_clear,
            g.wx_partly,
            g.wx_cloudy,
            g.wx_fog,
            g.wx_rain,
            g.wx_snow,
            g.wx_storm,
            g.wx_wind,
            g.block_full,
            g.block_top,
            g.block_bot,
            g.bar_fill,
            g.bar_empty,
        ]
        .into_iter()
        .chain(g.spin.iter().copied())
        {
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
            g.arrow_right,
            g.diamond_filled,
            g.diamond_hollow,
            g.role_server,
            g.role_client,
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
            g.jj,
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
            g.gauge,
            g.quote_open,
            g.quote_close,
            g.wx_clear,
            g.wx_partly,
            g.wx_cloudy,
            g.wx_fog,
            g.wx_rain,
            g.wx_snow,
            g.wx_storm,
            g.wx_wind,
            g.block_full,
            g.block_top,
            g.block_bot,
            g.bar_fill,
            g.bar_empty,
        ]
        .into_iter()
        .chain(g.spin.iter().copied())
        {
            let c = s.chars().next().unwrap();
            assert!(s.chars().count() == 1, "multi-char glyph: {s:?}");
            assert!((c as u32) <= 0xFFFF, "astral-plane glyph in chrome: {s:?}");
            assert_eq!(s.width(), 1, "glyph must be display-width 1: {s:?}");
        }
        assert_eq!(g.attention.width(), 2, "✋ is the sanctioned wide glyph");
    }

    #[test]
    fn spinner_frames_match_across_sets() {
        // The frame arrays must be the same length so a frame index derived
        // from elapsed time points at a valid frame in either set.
        assert_eq!(UNICODE.spin.len(), ASCII.spin.len());
        assert!(!UNICODE.spin.is_empty());
    }

    #[test]
    fn full_and_basic_share_unicode_glyphs() {
        assert_eq!(glyphs(UnicodeLevel::Full).box_tl, "╭");
        assert_eq!(glyphs(UnicodeLevel::Basic).box_tl, "╭");
        assert_eq!(glyphs(UnicodeLevel::Ascii).box_tl, "+");
    }

    #[test]
    fn every_glyph_token_resolves_across_all_sets() {
        // Every token maps to a non-empty glyph in both the Unicode and ASCII
        // sets — the exhaustive check that `Glyph::resolve` stays total and that
        // `Glyph::ALL` lists every variant (a new field with no arm would panic
        // the match compile; a variant missing from ALL is simply untested,
        // which coverage catches).
        for &g in Glyph::ALL {
            for level in [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii] {
                let s = g.resolve(glyphs(level));
                assert!(!s.is_empty(), "{g:?} resolves empty at {level:?}");
            }
        }
    }

    #[test]
    fn glyph_token_selects_ascii_fallback_at_the_chokepoint() {
        // The whole point: a token degrades to the ASCII field when the active
        // set is ASCII — no branching needed at the draw site. `resolve` returns
        // exactly the same `&'static str` a direct field read would.
        assert_eq!(Glyph::DotFilled.resolve(&UNICODE), UNICODE.dot_filled);
        assert_eq!(Glyph::DotFilled.resolve(&ASCII), ASCII.dot_filled);
        assert_eq!(Glyph::DotFilled.resolve(&ASCII), "*");
        assert_eq!(Glyph::Ellipsis.resolve(&ASCII), "...");
        assert_eq!(Glyph::BoxV.resolve(&ASCII), "|");
    }

    #[test]
    fn glyph_token_covers_every_glyphset_field() {
        // Guard against a `GlyphSet` field gaining no token: `ALL` must have one
        // token per single-string field. `spin` (the frame array) is the one
        // documented exclusion, so the count is the field total minus one.
        // (Kept as a concrete number so adding a field without a token trips it.)
        assert_eq!(Glyph::ALL.len(), 56);
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
    fn interpret_probe_xtqmodkeys_level_2_is_reportable() {
        let r = interpret_probe(b"\x1b[>4;2m");
        assert_eq!(r.modify_other_keys, Some(2));
        assert_eq!(r.ctrl_digit_reportable(), Some(true));
        assert!(r.responded);
    }

    #[test]
    fn interpret_probe_xtqmodkeys_level_1_is_not_reportable() {
        let r = interpret_probe(b"\x1b[>4;1m");
        assert_eq!(r.modify_other_keys, Some(1));
        assert_eq!(r.ctrl_digit_reportable(), Some(false));
    }

    #[test]
    fn interpret_probe_xtqmodkeys_level_0_is_not_reportable() {
        let r = interpret_probe(b"\x1b[>4;0m");
        assert_eq!(r.modify_other_keys, Some(0));
        assert_eq!(r.ctrl_digit_reportable(), Some(false));
    }

    #[test]
    fn interpret_probe_kitty_only_terminal_is_not_reportable() {
        // Answered the kitty query but not XTQMODKEYS: our modifyOtherKeys
        // push provably did nothing, and we never push the kitty protocol.
        let r = interpret_probe(b"\x1b[?0u");
        assert_eq!(r.kitty_keyboard, Some(0));
        assert_eq!(r.modify_other_keys, None);
        assert_eq!(r.ctrl_digit_reportable(), Some(false));
        assert!(r.responded);
    }

    #[test]
    fn interpret_probe_keyboard_silence_stays_unknown() {
        // Responded to DA but said nothing about the keyboard: unknown, which
        // callers must read as "assume it works".
        let r = interpret_probe(b"\x1b[?62;c");
        assert!(r.responded);
        assert_eq!(r.modify_other_keys, None);
        assert_eq!(r.kitty_keyboard, None);
        assert_eq!(r.ctrl_digit_reportable(), None);
    }

    #[test]
    fn interpret_probe_no_probe_is_unknown() {
        let r = interpret_probe(b"");
        assert_eq!(r, ProbeResult::default());
        assert_eq!(r.ctrl_digit_reportable(), None);
    }

    #[test]
    fn interpret_probe_da_is_not_a_kitty_reply() {
        // Same `ESC [ ?` prefix; only the terminator tells them apart.
        let r = interpret_probe(b"\x1b[?62;1;6c");
        assert_eq!(r.kitty_keyboard, None);
        assert_eq!(r.modify_other_keys, None);
        assert!(r.responded);
    }

    #[test]
    fn interpret_probe_full_batch_in_any_order() {
        let bytes = b"\x1b[?1u\x1bP>|ghostty 1.0\x1b\\\x1b[>4;2m\x1b[?62;22c";
        let r = interpret_probe(bytes);
        assert!(r.responded);
        assert!(r.modern);
        assert_eq!(r.terminal_name.as_deref(), Some("ghostty 1.0"));
        assert_eq!(r.kitty_keyboard, Some(1));
        assert_eq!(r.modify_other_keys, Some(2));
        assert_eq!(r.ctrl_digit_reportable(), Some(true));
    }

    #[test]
    fn interpret_probe_truncated_replies_degrade_to_unknown() {
        // Cut mid-number and cut before the terminator: neither may produce a
        // confident (and therefore wrong) answer.
        let cut_modkeys = interpret_probe(b"\x1b[>4;");
        assert_eq!(cut_modkeys.modify_other_keys, None);
        assert_eq!(cut_modkeys.ctrl_digit_reportable(), None);

        let cut_kitty = interpret_probe(b"\x1b[?0");
        assert_eq!(cut_kitty.kitty_keyboard, None);
        assert_eq!(cut_kitty.ctrl_digit_reportable(), None);
    }

    #[test]
    fn interpret_probe_unparsable_keyboard_values_stay_unknown() {
        // Out of `u8` range: "the terminal didn't tell us", not a guess.
        let r = interpret_probe(b"\x1b[>4;999m\x1b[?999u");
        assert!(r.responded);
        assert_eq!(r.modify_other_keys, None);
        assert_eq!(r.kitty_keyboard, None);
        assert_eq!(r.ctrl_digit_reportable(), None);
    }

    #[test]
    fn primary_da_terminator_ignores_the_other_replies() {
        // The real batch, in the order the probe asks for it. Only the DA ends
        // the read — a kitty reply shares the `ESC [ ?` prefix, and this
        // terminal's XTVERSION name carries a `c`.
        let kitty = b"\x1b[?0u";
        let modkeys = b"\x1b[>4;2m";
        let version = b"\x1bP>|Alacritty(0.15.1)\x1b\\";
        let da = b"\x1b[?6c";

        let mut buf = Vec::new();
        for part in [&kitty[..], &modkeys[..], &version[..]] {
            buf.extend_from_slice(part);
            assert!(
                !has_primary_da(&buf),
                "read must not stop before the DA arrives: {:?}",
                String::from_utf8_lossy(&buf)
            );
        }
        buf.extend_from_slice(da);
        assert!(has_primary_da(&buf));

        // A DA cut mid-flight is not a DA.
        assert!(!has_primary_da(b"\x1b[?6"));
        assert!(!has_primary_da(b""));
        // …and a DA on its own still terminates (the silent-keyboard case).
        assert!(has_primary_da(b"\x1b[?62;1;6c"));
    }

    #[test]
    fn keyboard_queries_end_with_an_sgr_reset() {
        assert_eq!(KEYBOARD_QUERIES, b"\x1b[?u\x1b[?4m\x1b[m");
        assert!(KEYBOARD_QUERIES.ends_with(b"\x1b[m"));
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
            ..ProbeResult::default()
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
