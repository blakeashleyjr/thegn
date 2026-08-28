# THE-70 chunk 2 — done

Commit: `cceca3b7` — `fix(the-70): app-tab intercept stole Ctrl+Alt chords; fix pane key encoding`

## Files changed (exactly three, as specified)

| file                                 | change                                                                                              |
| ------------------------------------ | --------------------------------------------------------------------------------------------------- |
| `crates/thegn-host/src/run.rs`       | the single app-tab-switch block + its comment (was `13981-14001`, now `13981-14011`). Nothing else. |
| `crates/thegn-host/src/input.rs`     | new `legacy_ctrl_byte`, rewritten `KeyCode::Char` arm, corrected Tab-arm comment, 5 new unit tests  |
| `crates/thegn-host/src/run_tests.rs` | 2 new tests appended next to the existing digit tests                                               |

`git status` is clean apart from this file; chunk 1's `thegn-core` files were
never touched.

## Part A — the app-tab intercept (`run.rs`)

Guard changed from `k.modifiers.contains(Modifiers::ALT)` to
`k.modifiers == Modifiers::ALT && app_host.tab_count() > 1`, and the
`tab_count() > 1` guard moved off the digit arm onto the whole `if` so it also
covers `]` / `[`. A1 and A2 in one edit, exactly as the spec's sample.

The `Some(target)` path (`ensure_app_loaded`, the focused-tile ownership branch)
is untouched. The surrounding comment now states the contract: exact-ALT only,
both arms gated on more than one tab, Alt+1..N still shadowing
`summon-worktree-N` for the first N tabs by design (chunk 3 makes the sidebar
hints agree).

## Part B — `key_bytes_mode` (`input.rs`)

New `fn legacy_ctrl_byte(c: char) -> Option<u8>`: `Some` only where the legacy
C0 mapping is actually defined — ASCII letters, `@ [ \ ] ^ _ ?`, space —
plus a passthrough arm for a char that is _already_ an ASCII control byte
(`< 0x20` or `0x7f`), which preserves today's behaviour bit-for-bit for a
terminal that reports the legacy spelling with the modifier still attached.
`None` for everything else → CSI-u `ESC [ <codepoint> ; <csi_u_modifier> u`.

ALT is applied as a `meta` closure that prepends `0x1b` to the non-ALT
encoding, on **both** the legacy-control-byte path and the plain-UTF-8 path,
and deliberately **not** on the CSI-u path (where `csi_u_modifier(mods)` already
carries the ALT bit).

The stale kitty-keyboard claim on the Tab arm is rewritten to say thegn forwards
CSI-u because no legacy byte exists, and to state explicitly that thegn does not
push `ESC [ >1u`.

## Deliberate deviation — `Ctrl+?`

The spec enumerates `?` as a character that _has_ a legacy control byte. The
correct byte is **DEL (`0x7f`)**; the old arithmetic produced `0x1f`, which is
`Ctrl+_`. I emit `0x7f`. This is a behaviour change on a chord the spec's test
list does not mention, so flagging it here and in the commit body: revert the
`'?' => Some(0x7f)` arm to `Some(0x1f)` if the review prefers strict
byte-identity over correctness on this one chord. Everything else the spec named
as "byte-identical to today" is verified identical by test.

## Tests added

`input.rs::tests` (pure):

- `ctrl_digit_is_never_the_xon_control_byte` — the named regression guard:
  asserts `Ctrl+1 != vec![0x11]`, then that it is `\x1b[49;5u`.
- `ctrl_digit_forwards_csi_u` — `Ctrl+0/1/2/9` ⇒ `\x1b[48;5u` / `49` / `50` / `57`.
- `ctrl_letter_and_legacy_punctuation_are_byte_identical` — `c`/`C`/`a` ⇒
  `0x03`/`0x03`/`0x01`; `[ ] \ ^ _ @` and space ⇒ `0x1b 0x1d 0x1c 0x1e 0x1f 0x00
0x00`; `?` ⇒ `0x7f`.
- `alt_char_is_esc_prefixed` — `Alt+w` ⇒ `[0x1b, b'w']`, `Alt+1` ⇒ `[0x1b, b'1']`,
  `Ctrl+Alt+c` ⇒ `[0x1b, 0x03]`, `Ctrl+Alt+1` ⇒ `\x1b[49;7u` (single ESC).
- `unmodified_chars_round_trip_utf8` — `w` ⇒ `b"w"`, `é` ⇒ its UTF-8 bytes.

`run_tests.rs` (dispatch, via the existing `dispatch_bytes` helper so the whole
parse → `normalize_key` → keymap path is exercised):

- `ctrl_alt_digits_reach_summon_pin` — `\x1b[<49..57>;7u` ⇒ `Action::SummonPin(1..9)`.
- `ctrl_alt_brackets_reach_strip_resize` — `\x1b[93;7u` ⇒ `GrowStrip`,
  `\x1b[91;7u` ⇒ `ShrinkStrip`.

No refactor of `run.rs` to make the intercept callable — per the spec, that
would widen the diff and collide with chunk 3.

## Verification run

- `just quick thegn-host` — clean, no warnings.
- `cargo nextest run -p thegn-host ctrl_digit alt_char ctrl_letter
unmodified_chars tab_forwards app_cursor mouse_encoding ctrl_alt summon`
  — **21 passed, 0 failed.** Includes all four pre-existing `input.rs` tests
  (`shift_tab_forwards_reverse_tab_sequence`, `modified_tab_forwards_csi_u`,
  `app_cursor_switches_arrows_to_ss3`, `mouse_encoding_honors_mode_and_format`)
  passing unchanged, plus the pre-existing keymap/summon tests.
- `cargo nextest run -p thegn-host normalize_key program_remap keybind_lock
attach` — **17 passed, 0 failed** (the other `key_bytes` consumers:
  `cmd/attach.rs:169`, the `program_remap` path at `run.rs:21324`, and
  `normalize_key_canonicalizes_kitty_csi_u_control_chars` at `run_tests.rs:1754`,
  which depends on the Shift+Tab byte).
- `rustfmt` applied to the three files; pre-commit `treefmt` passed at commit.

Per the dev-loop policy and the lead addenda, no `just test` / `just ci` /
`just coverage` / `just e2e` was run.

## Unverified

1. **The intercept guard change itself has no automated test.** It lives inline
   in the `event_loop` `match`; the spec forbids extracting it (chunk 3 collision).
   Verified by reading only. The keymap-reachability tests prove the chords are
   bound and dispatch correctly _if_ they reach the keymap — they cannot prove
   the intercept no longer eats them.
2. **No live/manual run.** `Ctrl+Alt+<digit>` → pin and `Ctrl+Alt+]`/`[` →
   strip resize were not exercised against a running thegn with `[apps]` tiles
   enabled (`tab_count() > 1`), nor was `Alt+]`/`Alt+[` falling through in the
   single-tab case.
3. **The pane-side behaviour change was not observed against a real child.**
   `Alt+<char>` forwarded to a pane is now `ESC` + char. Correct legacy meta
   encoding, but an inner app that ignores `ESC`+char now sees a stray ESC where
   it previously saw a bare character (design §4 risk 5). Untested against a
   real nested vim / readline / nested thegn behind the Ctrl+g keybind lock.
   Likewise the CSI-u forwarding of `Ctrl+<digit>` was not observed being
   _received_ by a child — only that thegn now emits it.
4. **`Ctrl+?` ⇒ `0x7f`** — see the deviation section above. Correct per the
   legacy mapping, but a change from today's `0x1f` that the spec did not
   explicitly sanction.
5. **Full-workspace gates not run** (`just test`, `just ci`, `just coverage`,
   `just e2e`) — per the addenda. Only the crate-scoped filters listed above.
   In particular, e2e snapshots were not checked; no chrome changed, so no
   baseline should have moved, but that is reasoning, not a run.
6. **Not checked against chunk 1 or chunk 3's work** — chunk 1 is in
   `thegn-core` (file-disjoint, no API dependency either way); chunk 3 edits
   `run.rs` at `resolve_termcaps` (~:126), the teardown (~:1073) and model
   wiring (~:11811). My `run.rs` diff is confined to the one block at
   `13981-14011`, so the regions do not overlap — but chunk 3's line numbers
   after `13981` shift by +10 lines from this commit.
