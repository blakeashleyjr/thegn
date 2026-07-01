# Add terminal compatibility & graceful degradation

## Summary

superzej rendered its chrome assuming a modern emulator: 24-bit color, full
Unicode, Nerd-Font glyphs. On the long tail of terminals we want to support —
bare `xterm`/`rxvt`, the Linux/BSD console, Termux, the Windows console,
`screen`/`tmux` passthrough, CI capture, and anything honoring `NO_COLOR` — that
produced wrong or broken output (truecolor SGRs sent unconditionally; hardcoded
box-drawing / dots / arrows / half-block logotype with no ASCII fallback; layout
math by char-count, so wide glyphs overflowed).

This change adds a unified terminal-capability layer that detects what the outer
terminal can do and **dynamically enables/disables features with graceful
fallbacks**, plus a `superzej doctor` command to make the result inspectable.

1. **`TermCaps` detection** (`superzej_core::termcaps`) — pure, env-based
   (`COLORTERM`/`$TERM`/`TERM_PROGRAM`/`VTE_VERSION`/`NO_COLOR`/`WT_SESSION`/
   locale) resolving a color depth (truecolor/256/16/none) and glyph level
   (full/basic/ascii), plus undercurl/mouse/osc52/sync flags.
2. **Color degradation** — the frame is composed in truecolor and quantized at
   the single `wire.rs` `color_spec` chokepoint to 256/16/none.
3. **Glyph degradation** — borders, status markers, and the logotype source
   their glyphs from an active `GlyphSet` (Unicode ↔ ASCII).
4. **Display-width correctness** — chrome layout uses `unicode-width`, not char
   count, so wide/CJK/emoji glyphs no longer overflow.
5. **Startup probe** — an optional, bounded, tty-gated DA + XTVERSION query
   refines detection for modern terminals reached over `ssh`/`tmux` that report
   a generic `$TERM`. Runs before the input reader exists, so it never spills
   into the loop and the first frame already reflects it.
6. **Config + diagnostics** — `[theme] color` / `glyphs` (auto|explicit) and
   `superzej doctor [--json]`.

## Impact

Roadmap items (tasks.md) this gives concrete behavior to:

- **Cross-platform reach** — complements the cross-platform `superzej-metrics` /
  `superzej-media` leaf crates and the `just check-cross` (darwin/windows) gate
  by making the _rendering_ substrate degrade rather than assume.
- **AI-free workspace shell** — strictly additive to the shell; no AI coupling.

Affected capabilities: a new `terminal-compat` capability; touches `rendering`
(color at the wire chokepoint) and `theming` (the `[theme]` knobs) without
changing their existing requirements.
