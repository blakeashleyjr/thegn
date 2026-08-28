# THE-70 — architect review verdict

**Branch:** `tg/the-70-summon-digits`
**Base:** `main` @ `982ab7cb` (already an ancestor — `git merge main` was a no-op,
nothing to resolve)
**Reviewed:** the full `git diff main...HEAD` across chunks 1–3
(`5196878a`, `cceca3b7`, `0b2ab6ee`, `8c9be42e`) plus my own fix commit.
**Date:** 2026-08-27

APPROVED

Three defects found and fixed in-branch (`c7047adb`). No revision chunk: nothing
left needs a coder round-trip. The residual list below is follow-up and
live-terminal verification, not rework.

---

## 1. Design compliance

Every decision in `architect/design.md` §2 is honoured, and the two root causes
the issue actually names are fixed at the right layer.

| decision | verdict |
| --- | --- |
| D1 — don't remap the workspace family | Honoured. Not touched; the rebind escape hatch is now documented and its ids are test-asserted to parse. |
| D2 — don't push the kitty protocol | Honoured. `run.rs:466-478` untouched; the stale claim in `input.rs`'s Tab comment was correctly rewritten. |
| D3 — probe, don't guess | Honoured. Same single bounded read, same `PROBE_BUDGET`, queries ride the existing batch — no new thread, timer, wake source, or round trip. |
| D4 — unknown never suppresses | Honoured and **pinned by name** (`workspace_digits_survive_unknown_and_supported_keyboards`). `ctrl_digit_reportable`'s `(None, None) => None` arm is the only path to unknown and no caller inverts it. |
| D5 — keep app-tab switching, make it narrow and honest | Honoured. Exact-ALT + `tab_count > 1` on both arms; the sidebar's `claimed_by_app_tabs` makes the hints agree without renumbering. |
| D6 — correct pane encoding, no negotiation | Honoured. `legacy_ctrl_byte` is the real C0 table; CSI-u only where no legacy byte exists; ALT is ESC-prefixed on the legacy and plain paths and not on CSI-u (where the modifier already rides in the param). |

Invariants: `thegn-core` stays substrate-free (pure parsing only); new state
landed on `ProbeResult`, not `TermCaps`, exactly as §3 predicted (no
`doctor.rs:3002` struct-literal breakage); `render_plan::plan` untouched;
`quick_jump_slots` is a pure function of `FrameModel`; no new ratchet entry
needed and all three help ratchets pass with the allowlists unmodified.

The three chunk summaries are accurate. I re-derived every claim I checked and
found no overstatement, including chunk 3's §4.4 verification and chunk 2's
byte-identity argument.

---

## 2. Fixed during review — `c7047adb`

### 2.1 The probe read loop stopped too early (introduced by chunk 3) — **correctness**

`probe.rs`'s terminator was "a `?` somewhere in the buffer, then any `c`". That
was sound while the only `?` came from the DA reply, which is asked for **last**.
`KEYBOARD_QUERIES` now puts a kitty reply (`ESC [ ? … u`) in *front* of it, so the
`?` lands early and the first `c` in the **XTVERSION name** ends the read.
`Alacritty` and `contour` both contain a `c`, are both in `MODERN_TERMS`, and both
speak the kitty protocol — i.e. the two terminals that trip this are exactly the
ones that answer the new query.

Failure mode: the read returns before the DA arrives, so **the DA bytes stay in
the tty and termwiz's reader thread decodes them as stray input** (the leak the
addendum asked about), and a version name cut mid-flight loses the `modern`
capability upgrade. It needs the replies to arrive in more than one `read()` —
likely over ssh/tmux, not on a local socket, which is why it would have shipped
looking fine.

Fixed with `thegn_core::termcaps::has_primary_da` — a strict
`ESC [ ? <digits and ';'> c` match reusing chunk 1's `csi_reply`. Test
`primary_da_terminator_ignores_the_other_replies` feeds the real four-reply batch
in arrival order and asserts the read does not stop until the DA lands. Every DA
reply in the wild (`\x1b[?6c`, `\x1b[?62;1;6c`, kitty's `\x1b[?62;c`, tmux's
`\x1b[?1;2c`) is digits-and-semicolons, so the strict rule cannot make a
responsive terminal wait out the budget.

### 2.2 The intercept guard now has a test — **the addendum's explicit ask**

Chunk 2 shipped the guard change with "verified by reading only", on the stated
grounds that extracting it would collide with in-flight chunk 3. That reason has
expired. The guard is now `apps::tab_chord(mods, key, tab_count) -> Option<TabChord>`
— a pure function in `apps/mod.rs` beside `tab_target`/`cycle`/`tab_count`, rather
than more `run.rs`. Behaviour is identical; the call site just destructures.

Four tests, covering precisely the bug class the issue names (bindings were
tested, delivery was not): exact-ALT claims `Alt+1..9`/`]`/`[`; `Ctrl+Alt+*`,
`Alt+Shift+*`, `Alt+Super+*` and bare digits are never claimed; both arms fall
through at 0 and 1 tabs; and the cycle deltas round-trip through the real
`AppHost::cycle` rather than being asserted in isolation.

### 2.3 The untouched `key_bytes` arms are now pinned — **test coverage**

The addendum asked for a table test over Tab/Enter/Backspace/arrows/Ctrl+letter.
Chunk 2 covered Ctrl+letter, Ctrl+punctuation, Tab and arrows; **Enter, Backspace,
Escape, Delete, Home and End had no test at all**, before or after. Added
`functional_keys_are_byte_identical`, including modified rows — those arms
deliberately ignore modifiers, and that is contract, not oversight. It is the
test that fails if someone later extends the ESC-meta prefix past the `Char` arm.

---

## 3. Verified, not changed

- **Byte identity of the rewritten `Char` arm.** Worked through against `main`'s
  `(c.to_ascii_uppercase() as u8).wrapping_sub(0x40) & 0x1f`: letters, `@ [ \ ] ^ _`
  and space are identical byte-for-byte. Exactly two intended changes —
  `Ctrl+<digit>`/`Ctrl+<no-legacy-byte>` → CSI-u instead of a fabricated control
  byte, and ALT → ESC prefix — plus the `Ctrl+?` deviation below.
- **`k.modifiers == Modifiers::ALT` is safe.** Checked termwiz 0.23.3's input
  parser: unix decoding yields bare `Modifiers::ALT` (`input.rs:911-936`), and the
  Windows console path (`modifiers_from_ctrl_key_state`, `:654-663`) folds
  `LEFT_ALT_PRESSED|RIGHT_ALT_PRESSED` into plain `ALT` too. The side-specific
  bits that appear in `LEFT_CTRL`-style tests are encode-side only, so the exact
  match cannot silently kill app-tab switching.
- **`csi_reply` cannot confuse DA with kitty.** It requires the terminator to sit
  immediately after the digit/`;` run, scans every occurrence of the prefix, and
  a truncated sequence simply fails to match — so a partial buffer degrades to
  unknown, never to a confident wrong answer. `u8` overflow → `None`, tested.
- **`\x1b[>4m` is right.** XTMODKEYS with `Pv` omitted resets the resource to its
  initial value. Correctly added to the panic path (which had no termwiz to lean
  on and was the actually-broken one, §1.7) and to the normal path for symmetry.
- **`app_tabs` before the first frame.** Re-derived on every full and bars-only
  recompose, and `len() <= 1 ⇒ claimed == 0` is the correct default regardless —
  pinned by `zero_or_one_app_tab_claims_no_worktree_digits`.
- **Doctor's remedy text.** Chunk 3's deviation from the design (`[keybinds]`, not
  `[keymap]`; `action-id = "Chord"`, not the reverse; `Ctrl Alt q` rather than
  `Alt Shift 1`, since Shift+digit yields punctuation on the very terminals the
  remedy addresses) is correct on all three counts. Accepted — the design was
  wrong here.
- **Help ratchets.** `cargo nextest run -p thegn-host help` — 71 passed. No
  `ACTION_SPECS` id added, no `actions:` frontmatter touched, no existing chord
  mention deleted, all three allowlists unmodified.

---

## 4. Accepted deviations

- **`Ctrl+?` → `0x7f` instead of `0x1f`** (chunk 2, flagged for a ruling). Keep it.
  DEL is the correct legacy byte; `0x1f` is `Ctrl+_`, which the old arithmetic
  produced by accident. Tested by name.
- **A fourth `run.rs` edit in chunk 3** — re-stamping `ctrl_digits_reportable`
  across the hydration model swap. Necessary and correct: `build_model` carries
  the `None` default, so setting it only in `main` would let the suppression last
  a fraction of a second. It uses the established LOOP-owned re-stamp idiom
  alongside `containers`/`dispatches`/`plugin_segments`, and copies a value rather
  than recomputing — the probe still runs exactly once.
- **`quick_jump_slots` extracted from `build_sidebar`.** Verbatim; it is what
  makes the suppression testable at all, since the slot vector never reaches
  `SidebarFrame`.

---

## 5. Follow-ups (none blocking)

1. **Live-terminal verification is the top item for the review/test stage.**
   Nothing on this branch has been answered by a real emulator: not the query
   shapes, not the `doctor` row rendered, not the panic restore. Every failure
   mode is one-directional by construction, so a misparse degrades to today's
   behaviour — with **one exception worth checking by hand**: a terminal that
   answers `CSI ? u` but stays silent on XTQMODKEYS while still honouring
   `CSI > 4 ; 2 m` would be scored `Some(false)` and lose its hints even though
   the chords work. Ghostty/WezTerm answer XTQMODKEYS (→ `Some(true)`, correct);
   kitty and Alacritty don't implement modifyOtherKeys at all (→ `Some(false)`,
   also correct); **foot is the one to check**. Worst case is a hint-only
   degradation that `thegn doctor` explains, never a dispatch break.
2. **`CSI ? u` read as `CSI u` (SCORC, restore cursor)** by a parser that drops
   the private marker — the same sloppiness class the trailing `\x1b[m` covers for
   `CSI ? 4 m`, but with no equivalent guard. The probe runs on the primary
   screen (before `?1049h`), so a spurious restore could leave the post-exit
   prompt at a stale position. Deliberately unmitigated: the obvious fix
   (`CSI s` first) is itself ambiguous with DECSLRM under `?69h`, i.e. it trades
   a hypothetical hazard for a real one. Cosmetic, hypothetical, documented here.
3. **`Ctrl+/` (and other punctuation with a de-facto mapping) now forwards CSI-u**,
   which the pane's child cannot read — it is inert where it was previously wrong
   (`0x0f`, Ctrl+O). No regression from working behaviour, but `'/' => Some(0x1f)`
   is a one-line strict improvement that the design's enumeration missed.
4. **The command palette still advertises `Ctrl+<digit>`** (`palette.rs`
   workspace/pin labels) now that the sidebar is honest. Chunk 3 flagged it; it
   was outside its file list. A label, not a dispatch — nothing is broken.
5. **`responded` widened.** A keyboard reply alone now sets it, so it means "the
   terminal said something", not "DA answered". `apply_probe` only branches on
   `modern`, so nothing behavioural follows today. Worth knowing before anyone
   leans on it.
6. **Slow-link leak.** Replies arriving after the 80 ms budget still leak into
   termwiz's reader; THE-70 adds two more sequences to what can leak. Pre-existing
   in kind (both are private CSI forms termwiz drops rather than turning into
   text), untested against a real slow link.
7. **D5's clean follow-up stands** — give app-tab switching real `ACTION_SPECS`
   actions instead of a pre-keymap intercept, and the `Alt+<digit>` collision (and
   the hint suppression that papers over it) goes away entirely.

---

## 6. Checks run

Scoped only, per the budget — no `just test`, `just ci`, `just coverage`, no e2e.

| check | result |
| --- | --- |
| `just quick thegn-core` | clean |
| `just quick thegn-host` | clean |
| `cargo clippy -p thegn-host --tests` | clean, no warnings |
| `cargo nextest run -p thegn-core termcaps` | **47 passed** |
| `cargo nextest run -p thegn-host` (input\|keymap\|probe\|doctor\|digit\|quick_jump\|gutter\|app_tab\|tab_chord\|keyboard\|summon\|normalize_key\|apps::) | **166 passed** |
| `cargo nextest run -p thegn-host help` | **71 passed** |
| `treefmt` on all touched files | clean |

**Frame-affecting changes, e2e not run.** The only painted difference is the
sidebar digit gutter, and it engages on `ctrl_digits_reportable == Some(false)`
— unreachable under `THEGN_E2E` (no tty ⇒ probe skipped ⇒ `None`) — or on more
than one app tab. `quick_jump_slots` is otherwise a verbatim extraction, and
`suppressing_a_digit_reserves_the_same_gutter` asserts identical
`(visible_index, y, height)` and equal row width with the digit shown vs hidden.
No baseline should move; re-record only if `just e2e` disagrees.

**Not gated here:** coverage (`thegn-core` 95% lines — new code is
`has_primary_da`, `ctrl_digit_reportable`, `csi_reply` and the two parse arms,
all with tests, but the gate was not measured), cross/feature/MSRV, nix-build,
deps-audit, openspec. The pre-push hook is the heavy gate.
