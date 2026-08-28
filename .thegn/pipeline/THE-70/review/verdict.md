# THE-70 — security / test / bug review verdict

**Branch:** `tg/the-70-summon-digits`
**Base:** `main` @ `982ab7cb` — already an ancestor, `git merge main` was a no-op,
nothing to resolve.
**Reviewed:** the full `git diff main...HEAD` (chunks 1–3 + the architect's
`c7047adb`), against `architect/design.md`, `architect-review/verdict.md` and all
three coder `Unverified` sections.
**Date:** 2026-08-27

PASS

One defect found and fixed in-branch (`35e9d848`). Six non-blocking findings
below — none of them breaks shipped behaviour, and every failure path I could
reach degrades one-directionally to today's behaviour.

The headline result of this stage is that **the branch's single biggest
untested surface is now tested**: the startup terminal probe has been driven
end-to-end against a scripted adversarial terminal (a real pty, the real
binary), covering the shapes chunk 3 listed as "reasoning, not observation".

---

## 1. Fixed during review — `35e9d848`

**The sidebar contradicted itself.** THE-70 makes the per-row quick-jump digits
honest (`quick_jump_slots` blanks the `Ctrl+<digit>` column on a proved-dead
terminal) — but the *same sidebar's* NAVIGATE footer went on printing

```
  Ctrl-1-9  jump workspace
```

a few lines underneath, because `sidebar_keytable::footer_hints` only ever saw
the config. Verified against the built binary (`thegn keys hints --zone
sidebar`). This is the exact surface the issue names ("doesn't work on the left
bar"), and it is the same class of lie the design's §1.4 calls out: _the UI
promises a chord the terminal cannot deliver._

`footer_hints` now takes the probe answer. Suppression matches
`quick_jump_slots`'s discipline and is **narrower** than it in one respect:

- only `Some(false)` hides; `None` (unknown — and `thegn keys hints`, which owns
  no tty to probe with) keeps every row (D4);
- only a Ctrl+`<digit>` chord qualifies (`needs_modified_key_reporting`).
  `Alt-1` has a legacy meta encoding, and a family rebound onto a letter —
  including `Ctrl Alt q`, the remedy `thegn doctor` itself prints — arrives
  fine, so neither is ever hidden.

Three tests, including the rebind case and the unknown/supported pair.

---

## 2. Live verification (architect follow-up #1 — the top open item)

Nothing on the branch had been answered by a real terminal. I built the binary
and drove `thegn doctor` under a pty while acting as the terminal, so the probe
write, the bounded read, the DA terminator, `interpret_probe` and the doctor row
all ran for real.

**Probe parsing / robustness** (`--json`, `keyboard` object):

| scenario | result |
| --- | --- |
| kitty + XTQMODKEYS=2 + XTVERSION `Alacritty(0.15.1)` + DA, **four separate writes** | `ctrl_digits_reportable: true`, `modify_other_keys: 2` |
| kitty-only (`CSI ? 0 u`) + version + DA | `false`, `modify_other_keys: null` |
| XTQMODKEYS level 1 + version + DA | `false`, `modify_other_keys: 1` |
| terminal silent (never answers) | `null`, returns inside the budget |
| binary garbage (C0 sweep + invalid UTF-8) then DA | `null` — no panic |
| out-of-range values (`CSI ? 999 u`, `CSI > 4 ; 999 m`) | `null` — no confident wrong answer |
| truncated replies, no DA at all | `null`, budget timeout |
| **user keystrokes interleaved** (`he` `ll` `o\r` between the replies) | `true` — typing does not corrupt the answer |
| replies **split mid-sequence** across five writes | `true` — the strict terminator waits |

Scenario 1 is the regression test for `c7047adb`: the version name contains a
`c` *and* a kitty reply puts a `?` in front of the DA, i.e. exactly the case the
old loose terminator would have cut short. It reads `modify_other_keys: 2`,
which is only possible if the read did **not** stop early — so the DA does not
stay in the tty to leak into termwiz's reader as stray keystrokes.

**The `doctor` row rendered** (plain output, not `--json`), all four states:

```
  keyboard      modifyOtherKeys=2 (Ctrl+<digit> chords OK)
  keyboard      not reported (Ctrl+1..9 / Ctrl+Alt+1..9 cannot reach thegn)
                use a terminal supporting xterm modifyOtherKeys level 2,
                  or rebind in [keybinds]: summon-workspace-1 … -9 and
                  summon-pin-1 … -9, e.g. summon-workspace-1 = "Ctrl Alt q"
  keyboard      not reported (…)                       [with TMUX set]
                inside tmux: set -g extended-keys on
                  (tmux 3.4+ also: set -as terminal-features '*:extkeys')
  keyboard      unknown (no probe — assuming supported)
```

Nothing garbled the output — the `\x1b[m` insurance and the private-marker
queries are invisible on a terminal that ignores them. `[keybinds]` and the
`action-id = "Chord"` direction are correct against `config.toml.example:862`.

**Real tmux** (3.7c, probe replayed inside it under a pty, with
`extended-keys on` and `off`) answers **neither** new query — only XTVERSION and
DA:

```
b'\x1bP>|tmux 3.7c\x1b\\\x1b[?1;2;4c'
```

→ `ctrl_digit_reportable() == None`. See finding 2.

**Panic-path restore:** verified by reading, not by inducing a panic. The change
is five bytes inside an existing `const SEQ`, written through the same
non-panicking `nix::unistd::write` + raw `tcsetattr`; nothing panicking was
added, so the "must never panic inside the panic handler" contract holds.
`\x1b[>4m` (XTMODKEYS with `Pv` omitted → reset to the initial value) is the
right sequence, and the panic path is the one that was genuinely broken.

---

## 3. Verified by re-derivation, not changed

- **Byte parity of the rewritten `Char` arm.** Worked through `main`'s
  `(c.to_ascii_uppercase() as u8).wrapping_sub(0x40) & 0x1f` against
  `legacy_ctrl_byte` for every input it accepts: letters, `@ [ \ ] ^ _`, space
  and the C0 pass-through arm (`c < 0x20`, where the old mask was a no-op) are
  **identical byte for byte**. Exactly three divergences, two of them
  deliberate: `Ctrl+?` → `0x7f` (accepted, and correct — `0x3f ^ 0x40 = 0x7f`),
  `Ctrl+<DEL char>` → `0x7f` instead of `0x1f` (undocumented but unreachable
  after `normalize_key` folds it to `KeyCode::Backspace`), and the intended
  CSI-u / ESC-meta changes.
- **The `meta` prefix cannot double up.** It wraps the legacy and plain-UTF-8
  branches only; the CSI-u branch carries ALT in `csi_u_modifier` instead
  (`Ctrl+Alt+1` → `\x1b[49;7u`, one ESC, asserted by test).
- **`k.modifiers == Modifiers::ALT` is safe.** Re-checked termwiz 0.23.3: the
  unix parser yields bare `Modifiers::ALT` (including the xterm "meta" params
  `;9`/`;11`, which alias to ALT), and the Windows console path folds
  `LEFT_ALT_PRESSED|RIGHT_ALT_PRESSED` into plain `ALT`. No side-specific bit
  can silently kill app-tab switching.
- **`csi_reply` is panic-free on hostile input.** Every slice index is a char
  boundary by construction (prefix matches and `find(predicate)` both land on
  boundaries), and it runs on `from_utf8_lossy` output. Confirmed live with a
  binary-garbage reply.
- **`claimed_by_app_tabs` is exact.** `tab_labels()` maps over `tab_order`, so
  `model.app_tabs.len() == app_host.tab_count()`, and `tab_target(idx)` returning
  `None` past the end makes the intercept fall through — so precisely `len()`
  digits are claimed, and `len() <= 1` claims none.
- **The new `FrameModel` field is correctly outside `hydration_eq`.** It is an
  explicit allowlist, the field is session-constant, and the re-stamp at
  `run.rs:9143-9145` happens *after* `model_changed` is computed — so it can
  neither force a repaint nor mask one. `model` is only ever wholesale-assigned
  at `run.rs:728` and `:9144`; both carry the value.
- **The pin strip is not a second lie.** It paints glyph+label chips with an
  *implicit* index (`chrome.rs:1090-1105`), no digits, so nothing there
  advertises `Ctrl-Alt-<digit>`. No suppression needed.
- **Exposure of the changed `Ctrl+<digit>` encoding is narrower than it looks.**
  `key_bytes_mode` is reached for a Ctrl+digit only via the `Ctrl+g` keybind
  lock, `thegn attach`, `program_remap`, or a digit the keymap does not claim
  (`Ctrl+0`) — because on a terminal that *can* report the chord the keymap
  claims it first, and on one that cannot, thegn never sees it.

---

## 4. Findings (all non-blocking)

**1 — `key_bytes_mode` emits CSI-u for chords that do have a de-facto legacy
byte, and CSI-u is not inert in every child.**
`crates/thegn-host/src/input.rs:47-70`. D6 says "CSI-u for combos that have **no
legal legacy control byte**", but the design's own §1.4 table enumerates the
xterm/console bytes for `Ctrl+2`→NUL, `Ctrl+3..7`→`0x1b..0x1f`, `Ctrl+8`→DEL,
and xterm special-cases `Ctrl+/`→`0x1f` (the architect's follow-up #3).
`legacy_ctrl_byte` returns `None` for all of them. Separately, the design's
"an unknown CSI is inert in every terminal app" is optimistic: a child that
doesn't decode CSI-u (readline, emacs) consumes `ESC [` as an unbound prefix and
**self-inserts the remainder** — `49;5u` typed into the buffer or command line.
That is the same spill the kitty decision (D2) exists to avoid.
_Not a regression_ — main fabricated a wrong control byte for every one of these
(`Ctrl+1`→XON, `Ctrl+/`→`0x0f`), so no working behaviour is lost, and the
exposure is the narrow set listed in §3. But it deserves one explicit ruling
rather than a half-application: either implement the xterm legacy table for the
whole class, or record that CSI-u is preferred because those bytes are
keyboard-layout-dependent. I deliberately did **not** apply the `'/' => 0x1f`
one-liner alone — picking one member of the class arbitrarily is worse than
either consistent answer, and this is the owner's call, not a review fix.

**2 — under tmux the probe cannot answer, so the feature's headline case is
undetected and its tmux remedy is unreachable.** Measured: tmux 3.7c replies to
neither `CSI ? u` nor `CSI ? 4 m`, with `extended-keys` on **or** off, so
`ctrl_digit_reportable()` is `None` either way. tmux-without-extended-keys is
the most commonly cited cause of "Ctrl+digit does nothing" and the reason
`keyboard_remedy(true)` exists — but that branch only runs on `Some(false)`,
which tmux never produces. So under tmux the digits stay painted and `doctor`
says "unknown (assuming supported)". This is D4-correct and degrades to today's
behaviour, but the tmux remedy is effectively dead code today. A cheap honest
improvement for a follow-up: when `TMUX` is set and the answer is `None`, say so
("unknown — tmux does not answer; if Ctrl+`<digit>` does nothing, set -g
extended-keys on") instead of "assuming supported".

**3 — `\x1b[>4m` on the *normal* teardown is decorative.**
`crates/thegn-host/src/run.rs:1096`. It is written, and then `set_cooked_mode()`
below it asks termwiz for `modify_other_keys(1)`, which overwrites the reset. The
comment ("keep the two symmetrical") is honest about the intent, but a reader
could take it as the reset actually taking effect. Harmless; the panic path —
the one §1.7 identified as broken — is correct and unconditioned.

**4 — `interpret_probe`'s `responded` flag still uses the loose DA rule.**
`crates/thegn-core/src/termcaps.rs:986` is `find("\x1b[?")` + `contains('c')`,
i.e. exactly the rule `has_primary_da` was introduced to replace. With a kitty
reply now in the buffer it can be satisfied by the kitty reply plus any `c` in
the XTVERSION name. `responded` only feeds `doctor`'s `answered` row and a
tracing field (nothing branches on it — `apply_probe` reads only `modern`), so
this is cosmetic; but the two rules living side by side in one function is a
trap for the next editor. Reusing `has_primary_da` there is a one-line tidy.

**5 — `quick_jump_slots` suppresses without checking the family is still on
Ctrl+`<digit>`.** `crates/thegn-host/src/sidebar_view.rs:583`. A user who takes
`doctor`'s advice and rebinds `summon-workspace-1..9` onto a deliverable chord
still loses the row digits. Narrow (requires both a rebind and a proved-dead
terminal) and arguably fine, since the row digit is a slot number rather than a
chord — but the footer fix in `35e9d848` does make this distinction, so the two
now differ. Worth reconciling if anyone touches it.

**6 — `probe_outer_terminal_cli` leaves the terminal at `modifyOtherKeys = 1`.**
`crates/thegn-host/src/probe.rs:72-77` enters raw mode (termwiz pushes level 2)
and leaves via `set_cooked_mode()` (level 1), never the `\x1b[>4m` reset this
branch added elsewhere. Pre-existing, not introduced here, and level 1 is
benign; noted because `thegn doctor` is now the tool people will run *about*
this exact resource.

Carried forward unchanged from the architect's list and re-confirmed as
non-blocking: `CSI ? u` misread as SCORC by a marker-dropping parser (#2),
palette labels still advertising `Ctrl+<digit>` (#4), the widened `responded`
(#5, see finding 4), the slow-link reply leak (#6), and D5's ACTION_SPECS
follow-up (#7).

---

## 5. Checks run

Scoped only, per the addenda — no `just test`, `just ci`, `just coverage`, no e2e.

| check | result |
| --- | --- |
| `git merge main` | already up to date (no-op) |
| `cargo nextest run -p thegn-core termcaps` | **47 passed** |
| `cargo nextest run -p thegn-host` (input\|keymap\|probe\|doctor\|digit\|quick_jump\|gutter\|tab_chord\|keyboard\|summon\|normalize_key\|slot\|app_tab) | **171 passed** |
| `cargo nextest run -p thegn-host` (sidebar_keytable\|sidebar_view\|input\|apps::\|doctor\|ratchet\|help\|summon\|tab_chord), after the fix | **187 passed** |
| `cargo clippy -p thegn-core -p thegn-host --tests --all-features` | clean, no warnings |
| Rust ratchets (platform-cfg, color, glyph, host-key) | pass |
| shell ratchets: `ignored-result` (323 pinned), `async-trait`, `element` | clean, allowlists unmodified |
| help ratchets (`-E test(help)`) | pass, all three allowlists unmodified |
| `treefmt` on the touched files | clean |
| `just smoke` (incl. `test/pty-smoke.sh`) | **all checks passed**, incl. PTY launch → first frame at 100x30 and 40x8 |
| live pty probe harness (13 scenarios, real binary) | see §2 |

**Frame-affecting changes; e2e not run.** Two painted differences, both gated on
states unreachable under `THEGN_E2E` (no tty ⇒ probe skipped ⇒ `None`) or on
more than one app tab: the sidebar digit gutter (geometry pinned by
`suppressing_a_digit_reserves_the_same_gutter`) and, from `35e9d848`, one fewer
NAVIGATE footer row. No baseline should move; re-record only if `just e2e`
disagrees.

**Not gated here:** `thegn-core`'s 95% coverage gate (the new core code —
`has_primary_da`, `csi_reply`, `ctrl_digit_reportable`, the two parse arms —
each has tests, but `cargo llvm-cov` was not run), cross/feature/MSRV,
nix-build, deps-audit, openspec, e2e. The pre-push hook is the heavy gate.

Ready for the merge queue.
