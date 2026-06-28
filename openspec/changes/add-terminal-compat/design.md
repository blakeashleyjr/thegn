# Design

## Why a single capability layer

The codebase already had the shape: `undercurl_supported_env` (pure, env-based,
tested) → resolver folding config over detection → a render-time global read
lock-free during render (the `seg` undercurl atomic; the chrome `PALETTE`
`RwLock`). This change generalizes that one pattern into `TermCaps` rather than
inventing a new mechanism, so detection stays pure/testable in core and the
host owns only the I/O seams.

## Key decisions

- **Color quantization at one chokepoint.** Every chrome and pane color reaches
  the wire through `wire::color_spec`. Making it depth-aware covers everything
  with no per-call-site change; it runs inside `emit_attrs`, which already
  dedups unchanged SGR runs, so the cost is per-style-run, not per-cell. The
  `Panes`/bounded-diff render shape is preserved (no extra recompose).

- **Glyphs via a `&'static GlyphSet` behind an atomic**, not threaded through
  every draw signature. Borders/chrome/pins/logotype are free functions taking
  `Surface` + style; plumbing a glyph ref through all of them would be invasive
  and against the grain. Reads are a branchless atomic load + const reference —
  safe on the hot path, matching the sanctioned undercurl/palette globals.

- **Probe before the input reader, not after the first frame.** termwiz 0.23
  can't surface DA/XTVERSION replies (they spill as key events — the same limit
  that disables the kitty keyboard protocol), and its single reader thread owns
  the tty once `BufferedTerminal` is built. So the probe reads the raw fd in the
  startup window after `set_raw_mode()` and before `BufferedTerminal::new`. This
  is even better than the originally-considered post-first-frame probe: the
  first frame already reflects the result (no re-render flash), the event loop
  never sees response bytes, and a short tty-gated deadline bounds the worst
  case (a non-responding terminal) well under the launch budget.

- **Test isolation via `cfg(test)` thread-local overrides.** The capability
  holder is a process-wide global (it must cross the probe/reload threads), but
  cargo runs each test on its own thread, so a thread-local override gives
  per-test isolation without ever mutating the shared atomics — no flaky
  cross-test races between glyph/color-asserting tests.

## Alternatives considered

- **termwiz's stock 256-quantizer**: termwiz 0.23 exposes none, so the standard
  6×6×6-cube + grayscale-ramp formula is ported (pure, unit-tested).
- **Post-first-frame async probe** (the initial preview): rejected because
  termwiz can't cleanly read the reply and its reader owns the tty by then; the
  pre-reader startup probe is strictly safer and flash-free.
