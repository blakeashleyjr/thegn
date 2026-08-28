# THE-70 chunk 2 — pre-keymap dispatch defects + pane key encoding (`thegn-host`)

Read `.thegn/pipeline/THE-70/architect/design.md` §1.5, §1.6, §2 (D5/D6), §4
before starting. Every defect below is confirmed by reading the code, not
inferred — the citations are exact.

## Files touched (exact, exhaustive)

- `crates/thegn-host/src/run.rs` — **one block only**, lines 13988-14001 (the
  app-tab switch `match`). Nothing else in this 20k-line file.
- `crates/thegn-host/src/input.rs` — the `KeyCode::Char` arm of
  `key_bytes_mode` (`input.rs:62-70`), the stale comment at `input.rs:74-77`,
  and the file's own `#[cfg(test)] mod tests`.
- `crates/thegn-host/src/run_tests.rs` — new tests only, appended.

## Overlap / dependency

- **Parallel-safe with chunk 1** (different crate, no shared file, no API
  dependency in either direction).
- **Chunk 3 also edits `run.rs`** (a different region: `resolve_termcaps` near
  line 126, the teardown near line 1073, and model wiring near line 11811).
  Chunk 3 runs **after** this one. Keep your `run.rs` diff to the single block
  named above so the two never collide.
- No signature changes anywhere. In particular, do **not** change
  `key_bytes` / `key_bytes_mode`'s signature — that would force `run.rs:21324`
  and `run.rs:16168` edits and break the file-disjointness above.

## Approach

### Part A — the app-tab intercept steals three keymap families (`run.rs:13988-14001`)

Current code:

```rust
let target = if k.modifiers.contains(Modifiers::ALT) {
    match k.key {
        KeyCode::Char(c @ '1'..='9') if app_host.tab_count() > 1 => {
            let idx = (c as usize) - ('1' as usize);
            app_host.tab_target(idx)
        }
        KeyCode::Char(']') => Some(app_host.cycle(app_host.active, 1)),
        KeyCode::Char('[') => Some(app_host.cycle(app_host.active, -1)),
        _ => None,
    }
} else { None };
```

This block runs before every zone handler and `continue`s at `run.rs:14014`, so
anything it claims never reaches `keymap.dispatch` (`run.rs:18441`).

**A1 — match ALT exactly, not "contains ALT".** `contains(Modifiers::ALT)` is
true for `Ctrl+Alt`, so `Ctrl+Alt+1..9` → `Action::SummonPin(1..9)`
(`keymap.rs:1428-1430`, tested at `keymap.rs:2320`) is swallowed whenever
`tab_count() > 1`. Change the guard to an exact match on `Modifiers::ALT`.

**A2 — guard the `]` / `[` arms on `tab_count() > 1`.** `AppHost::cycle`
(`apps/mod.rs:209-220`) always returns a target, so those arms unconditionally
`continue`. Combined with A1's bug that makes `Ctrl Alt ]` → `Action::GrowStrip`
and `Ctrl Alt [` → `Action::ShrinkStrip` (`keymap.rs:1434-1435`) **permanently
unreachable on every configuration**, and it also eats plain `Alt+]`/`Alt+[` in
the single-tab case the comment at `run.rs:13983-13987` claims to have ceded.

Both fixes together, e.g.:

```rust
// Exact-ALT: `Ctrl+Alt+<digit>` is `summon-pin-N` and `Ctrl+Alt+]`/`[` are
// GrowStrip/ShrinkStrip — different families that must reach the keymap.
// `tab_count() > 1` guards BOTH arms: with a lone Work tab there is nothing
// to switch or cycle between, and `cycle` would otherwise claim the key and
// return the tab we are already on.
let target = if k.modifiers == Modifiers::ALT && app_host.tab_count() > 1 {
    match k.key {
        KeyCode::Char(c @ '1'..='9') => {
            app_host.tab_target((c as usize) - ('1' as usize))
        }
        KeyCode::Char(']') => Some(app_host.cycle(app_host.active, 1)),
        KeyCode::Char('[') => Some(app_host.cycle(app_host.active, -1)),
        _ => None,
    }
} else { None };
```

Update the surrounding comment (`run.rs:13981-13987`) to state the new contract:
exact-ALT only, both arms gated on more than one tab, and Alt+1..N still
shadowing `summon-worktree-N` for the first N tabs by design (chunk 3 makes the
sidebar hints agree).

**Do not** change what happens once `target` is `Some` — leave
`ensure_app_loaded` / the focused-tile ownership branch at `run.rs:14016-14026`
alone.

### Part B — `key_bytes_mode` mangles Ctrl+non-letter and drops Alt (`input.rs:62-70`)

```rust
KeyCode::Char(c) => {
    if mods.contains(Modifiers::CTRL) {
        let b = (c.to_ascii_uppercase() as u8).wrapping_sub(0x40);
        Some(vec![b & 0x1f])
    } else { /* raw UTF-8 */ }
}
```

The arithmetic assumes an ASCII letter. `Ctrl+1` → `0x31 - 0x40 = 0xF1`,
`& 0x1f` = `0x11` — **Ctrl-Q, i.e. XON**, which can unfreeze/freeze flow control
in a child. `Ctrl+2` → `0x12`, `Ctrl+0` → `0x10`, `Ctrl+9` → `0x19`. Separately
ALT is silently dropped for `Char`; it only feeds `csi_u_modifier`
(`input.rs:26-41`), used solely by the `Tab` arm.

This is the live path for nested sessions: with the Ctrl+g keybind lock engaged
(`run.rs:16150-16174`) every key is forwarded through this function regardless of
zone, and panes run under `TERM=xterm-256color` (`pane_pty.rs:92`).

**B1 — only emit a legacy control byte when one legally exists.** The legacy
C0 mapping is defined for ASCII letters and for `@ [ \ ] ^ _ ?` (and space →
NUL). For any other `Char` with CTRL, emit the CSI-u form instead —
`ESC [ <codepoint> ; <csi_u_modifier(mods)> u` — mirroring the existing `Tab`
arm at `input.rs:78-80`. An unknown CSI is inert in every terminal app; today's
byte is actively wrong.

**B2 — ESC-prefix Alt.** For `KeyCode::Char` with ALT, emit `0x1b` followed by
the non-ALT encoding (the raw UTF-8 bytes, or the legacy control byte when CTRL
is also held). This is the standard legacy meta encoding. When the CTRL case
falls to B1's CSI-u form, the ALT bit is already carried by
`csi_u_modifier(mods)` — do **not** also prepend ESC in that case, or the child
sees a doubled modifier.

Keep Ctrl+letter and plain characters byte-identical to today — vim, readline
and every TUI depend on those exact bytes. Do not touch the `Tab`, `Enter`,
arrow, `Escape`, `Home`/`End`, `Delete` or `Backspace` arms.

**B3 — fix the stale comment at `input.rs:74-77`.** It claims the CSI-u form is
disambiguated by "the host's own kitty-keyboard mode (`ESC [ >1u`)". thegn does
**not** push the kitty protocol — `run.rs:466-478` says so explicitly and gives
the reason. Rewrite it to say thegn forwards CSI-u because no legacy byte
exists for that chord, not because of a mode it pushed.

## Tests

### In `input.rs`'s `mod tests` (unit, pure)

- `Ctrl+1` ⇒ `\x1b[49;5u` (not `0x11`). Same for `Ctrl+2` ⇒ `\x1b[50;5u` and
  `Ctrl+9` ⇒ `\x1b[57;5u`.
- `Ctrl+0` ⇒ `\x1b[48;5u` (not `0x10`).
- **Regression guard, name it clearly:** assert `Ctrl+1` is NOT `vec![0x11]` —
  the XON hazard is the reason this change exists.
- Ctrl+letter unchanged: `Ctrl+c` ⇒ `[0x03]`, `Ctrl+a` ⇒ `[0x01]`,
  `Ctrl+C` (uppercase) ⇒ `[0x03]`.
- Legacy-legal punctuation unchanged: `Ctrl+[` ⇒ `[0x1b]`, `Ctrl+]` ⇒ `[0x1d]`,
  `Ctrl+\` ⇒ `[0x1c]`, `Ctrl+space` ⇒ `[0x00]`.
- `Alt+w` ⇒ `[0x1b, b'w']`; `Alt+1` ⇒ `[0x1b, b'1']`.
- `Ctrl+Alt+c` ⇒ `[0x1b, 0x03]`.
- `Ctrl+Alt+1` ⇒ `\x1b[49;7u` (mod = 1 + alt(2) + ctrl(4)) — **single** ESC,
  no doubled prefix.
- Plain `w` ⇒ `[b'w']`; a non-ASCII char (e.g. `é`) with no modifiers still
  round-trips its UTF-8 bytes.
- Existing tests (`shift_tab_forwards_reverse_tab_sequence`,
  `modified_tab_forwards_csi_u`, `app_cursor_switches_arrows_to_ss3`,
  `mouse_encoding_honors_mode_and_format`) must pass unchanged.

### In `run_tests.rs` (dispatch, appended)

There is an existing pattern to copy: `dispatch_bytes` at `run_tests.rs:1626`.
Add pure keymap-level tests next to the existing digit tests
(`run_tests.rs:1636-1683`) proving the families the app-tab block used to steal
are reachable in the keymap:

- `Ctrl+Alt+1..9` ⇒ `Action::SummonPin(n)`.
- `Ctrl+Alt+]` ⇒ `Action::GrowStrip`; `Ctrl+Alt+[` ⇒ `Action::ShrinkStrip`.

For the intercept itself: the block lives inline in `run()` and is not directly
callable. Do **not** refactor `run.rs` to extract it — that widens the diff and
collides with chunk 3. Cover it by the keymap-reachability tests above (they
prove the chords are bound and would fire if they got there), and say plainly in
the commit body that the guard change itself is verified by reading, not by an
automated test.

## Tests to run (scoped — do NOT run a full-workspace gate)

```sh
just quick thegn-host
cargo nextest run -p thegn-host input::
cargo nextest run -p thegn-host summon
cargo nextest run -p thegn-host key_bytes
```

Do **not** run `just test`, `just ci`, `just coverage`, or `just e2e`.

## Done criteria

- Exactly three files changed: `run.rs` (one block + its comment), `input.rs`,
  `run_tests.rs`.
- `Ctrl+Alt+<digit>`, `Ctrl+Alt+]` and `Ctrl+Alt+[` reach the keymap regardless
  of how many app tabs exist; `Alt+]`/`Alt+[` no longer swallow the key when
  there is a single tab.
- `key_bytes(Ctrl+'1')` is `\x1b[49;5u`, never `0x11`; Ctrl+letter and
  legacy-legal punctuation are byte-identical to before; Alt is ESC-prefixed.
- The stale kitty-keyboard claim at `input.rs:74-77` is corrected.
- `just quick thegn-host` clean; all listed tests pass.

**Commit subject (exact):**

```
fix(the-70): app-tab intercept stole Ctrl+Alt chords; fix pane key encoding
```

In the commit body: name the three intercept defects with their line numbers,
state the `0x11`/XON hazard, and **flag the behaviour change** — Alt+char
forwarded to a pane is now ESC-prefixed (correct legacy meta encoding), so an
inner app that ignores `ESC`+char sees a stray ESC where it previously saw a
bare character (design §4 risk 5).
