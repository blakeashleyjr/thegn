# THE-70 — architect design

**Issue:** Ctrl+`<digit>` summon doesn't work on the left bar; audit Alt+`<digit>` too.
**Branch:** `tg/the-70-summon-digits`
**Date:** 2026-08-27

---

## 1. What was investigated

Traced the whole path for all three summon families — terminal encoding →
termwiz decode → `normalize_key` → pre-keymap interception → `keymap.dispatch`
→ action arm → pane forwarding — and read the sidebar hint painter that
advertises the chords.

Everything below is cited. Nothing here is a hypothesis.

### 1.1 The keymap and the actions are correct

- Families registered in a loop from three constants:
  `keymap.rs:411-413` (`SUMMON_WORKTREE_MOD = "Alt"`,
  `SUMMON_WORKSPACE_MOD = "Ctrl"`, `SUMMON_PIN_MOD = "Ctrl Alt"`),
  bound at `keymap.rs:1409-1430` via `insert_all` (all four modes).
- Unit tests pin the bindings: `keymap.rs:2301` / `:2320` / `:2339`.
- Action arms exist and are correct:
  `SummonWorkspace` `run.rs:19768` (via `summon_workspace_target`,
  `run.rs:1451-1465`), `SummonWorktree` `run.rs:19818`,
  `SummonPin` `run.rs:20957`.

### 1.2 The sidebar is NOT swallowing the chords

The sidebar zone branch is explicitly modifier-guarded — `run.rs:16262-16271`:

```rust
if forced_palette_action.is_none()
    && focus.sidebar()
    && !k.modifiers.contains(Modifiers::CTRL)
    && !k.modifiers.contains(Modifiers::ALT)
```

and `sidebar_keytable::resolve` (`sidebar_keytable.rs:368-383`) has **no digit
entries** in `SIDEBAR_KEYS` (`sidebar_keytable.rs:145-340`), so digits return
`NotHandled` and fall through (`run.rs:6836-6839`). The issue's "prime suspect"
(panel focus / sidebar zone routing) is **ruled out**.

### 1.3 Decoding is correct — _when the bytes arrive_

`run_tests.rs:1637-1667` already drives raw bytes through
`termwiz::InputParser` → `normalize_key` → `default_keymap().dispatch()` and
proves `CSI 49;5u` / `CSI 27;5;50~` reach `SummonWorkspace(1..9)`.
`run_tests.rs:1670-1683` documents the legacy failure explicitly.

### 1.4 ROOT CAUSE A — Ctrl+`<digit>` is undeliverable on some terminals, and thegn never notices or says so

Legacy terminal encoding has no distinct byte for Ctrl+1..9. thegn's only
disambiguation is xterm `modifyOtherKeys` level 2, pushed by termwiz's
`set_raw_mode` (`termwiz-0.23.3/src/terminal/unix.rs:339` →
`:104-113`, emitting `CSI > 4 ; 2 m`). thegn deliberately does **not** push the
kitty keyboard protocol, and `run.rs:466-478` documents why (termwiz 0.23.3
cannot decode kitty CSI-u with sub-parameters; ghostty's event-type form spilled
literal characters into the focused pane). **That decision stands — do not
reopen it.**

Consequence on a terminal that ignores `CSI > 4 ; 2 m` (Alacritty, tmux without
`extended-keys on`, Linux console, older VTE, Terminal.app, and — for the
`>= 2` level specifically — kitty-protocol-only emulators):

| chord     | legacy byte    | what thegn sees                                                                           |
| --------- | -------------- | ----------------------------------------------------------------------------------------- |
| Ctrl+1    | `0x31`         | plain `1` — nothing happens                                                               |
| Ctrl+2    | `0x00`         | NUL → `normalize_key` (`run.rs:4455-4461`) rewrites to **Ctrl+Space** → opens the palette |
| Ctrl+3    | `0x1b`         | **Escape**                                                                                |
| Ctrl+4..7 | `0x1c`..`0x1f` | junk                                                                                      |
| Ctrl+8    | `0x7f`         | **Backspace**                                                                             |
| Ctrl+9    | `0x39`         | plain `9`                                                                                 |

So it is not merely inert: it _misfires_. And the same class kills
`Ctrl+Alt+<digit>` (pins), which has no legacy encoding at all.

Meanwhile the sidebar paints the Ctrl digit hints **unconditionally** whenever
it is focused (`sidebar_view.rs:558-578` assigns slots;
`sidebar_view.rs:1388-1395` paints the workspace digit) — i.e. the UI promises a
chord the terminal cannot deliver. That is exactly the reported symptom: digits
visible on the left bar, pressing them does nothing.

thegn already probes the outer terminal at startup (`probe.rs`, DA + XTVERSION,
raw-tty-gated, `PROBE_BUDGET` 80 ms, `run.rs:512`) and already folds the result
into caps (`termcaps::interpret_probe` `termcaps.rs:900-927`, `apply_probe`
`:933-953`). **It just never asks about the keyboard.** That is the gap.

### 1.5 ROOT CAUSE B — Alt+`<digit>` and Ctrl+Alt+`<digit>` are stolen before the keymap

`run.rs:13988-14001`, the top-level app-tab switch block, runs _before_ every
zone handler:

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

Three defects, all provable:

1. **`contains(ALT)` is not `== ALT`.** `Ctrl+Alt+1..9` satisfies it, so
   `SummonPin(1..9)` (`keymap.rs:1428-1430`, tested at `keymap.rs:2320`) is
   swallowed by the app-tab switcher whenever `tab_count() > 1`. Pins silently
   stop working the moment any `[apps]` tile is enabled.
2. **The `]` / `[` arms have no `tab_count() > 1` guard**, and
   `AppHost::cycle` (`apps/mod.rs:209-220`) _always_ returns a target (it
   returns `ActiveApp::Work` for an empty order and wraps otherwise). Combined
   with defect 1 this makes `Ctrl Alt ]` → `Action::GrowStrip` and
   `Ctrl Alt [` → `Action::ShrinkStrip` (`keymap.rs:1434-1435`) **permanently
   unreachable**, on every configuration — the block `continue`s at
   `run.rs:14014` before the keymap is ever consulted. Plain `Alt+]`/`Alt+[`
   also eat the keystroke in the single-tab case the comment at
   `run.rs:13983-13987` claims to have ceded.
3. **Alt+1..N genuinely collides** with `summon-worktree-N` when
   `tab_count() > 1` (N = number of app tabs). `tab_target` returns `None` past
   the end (`apps/mod.rs:198-200`), so only the first N digits collide — but the
   sidebar paints worktree digits `1..9` regardless
   (`sidebar_view.rs:566-574`, `:1503-1506`), so hints 1..N are a lie.

This is precisely the failure mode the issue names: _the tests pin the
bindings, not the delivery._ Every one of these bugs lives upstream of
`keymap.dispatch` (`run.rs:18441`), where no keymap unit test can see it.

### 1.6 ROOT CAUSE C — pane/nested-session relay mangles Ctrl+`<digit>` and drops Alt

`input.rs:62-70`:

```rust
KeyCode::Char(c) => {
    if mods.contains(Modifiers::CTRL) {
        let b = (c.to_ascii_uppercase() as u8).wrapping_sub(0x40);
        Some(vec![b & 0x1f])
    } else { /* raw UTF-8 */ }
}
```

The arithmetic assumes an ASCII letter. For digits it produces a **valid but
wrong** control byte: `Ctrl+1` → `0x11` (**Ctrl-Q — XON, which un-freezes/freezes
flow control in many stacks**), `Ctrl+2` → `0x12`, `Ctrl+0` → `0x10`,
`Ctrl+9` → `0x19`. Separately, **ALT is silently discarded** for `Char` — it
only participates in `csi_u_modifier` (`input.rs:26-41`), which is used solely
by the `Tab` arm (`input.rs:78-80`). So `Alt+w` forwarded to a pane arrives as a
bare `w`.

This is the issue's checklist item (2) _"relay for nested sessions"_: panes run
under `TERM=xterm-256color` (`pane_pty.rs:92`) and the live path where an inner
app gets raw keys is the Ctrl+g keybind lock (`run.rs:16150-16174`, which calls
`key_bytes_mode` for **every** key regardless of zone). A nested thegn/vim
behind Ctrl+g therefore receives `0x11` for Ctrl+1 and a bare `1` for Alt+1.

Note also that the comment at `input.rs:76` — _"the host's own kitty-keyboard
mode (`ESC [ >1u`)"_ — is **stale/false**: `run.rs:466-478` says thegn does not
push it.

### 1.7 ROOT CAUSE D — modifyOtherKeys is not restored on the panic path

The normal teardown calls `set_cooked_mode()` (`run.rs:1082`), which termwiz
implements as `modify_other_keys(1)` (`termwiz .../unix.rs:345-349`). The
**panic** restore does not: `platform/unix.rs:58` writes
`\x1b[?1006l\x1b[?1002l\x1b[?7h\x1b[<u\x1b[?25h\x1b[?1049l` then a raw
`tcsetattr`. So after a thegn panic the user's shell is left in
`modifyOtherKeys = 2` and readline sees CSI-u sequences it cannot parse. One-line
fix, directly adjacent to this issue's subject matter.

(Related, deliberately **not** changed: both restore strings pop the kitty
keyboard stack with `\x1b[<u` though thegn never pushes it —
`run.rs:1073`, `platform/unix.rs:58`. Popping an empty stack is a documented
no-op in the kitty spec and it is defensive against an inner app that leaked
flags, so it stays. It just deserves a comment saying so.)

### 1.8 THE-75 cross-check: `parse_chord` case rule is already settled on main

The issue asks to verify before designing. `keymap.rs:1130-1143` now carries an
explicit doc-comment: letter case is significant and means Shift, `"Alt W"` is
Alt+Shift+w, `"Alt Shift s"` and `"Alt S"` are the same chord, and — spelled out
verbatim — _"the consequence worth knowing: `\"Ctrl Alt M\"` requires
Ctrl+Alt+**Shift**+m."_ The defaults use the pairs deliberately
(`t`/`T`, `n`/`N`, `x`/`X`, `Ctrl Alt p`/`Ctrl Alt P` at
`keymap.rs:1436-1437`). **This is documented intended behaviour, not a bug, and
is out of scope for THE-70.** Digit tokens are unaffected — `Key::modified`
(`sequence.rs:34-47`) only folds SHIFT for ASCII-alphabetic chars.

---

## 2. Decisions

**D1 — Do not remap the workspace family off Ctrl+`<digit>`.**
It is the universal cross-application idiom (browsers, VS Code, iTerm, every
multiplexer), it works on every terminal thegn's own docs call modern once
`modifyOtherKeys`/CSI-u is on, and users can already rebind it today:
`Action::from_key` parses `summon-workspace-N` / `workspace-N`
(`keymap.rs:766-790`) and `[keymap]` binds flow through `keymap.rs:1639`. The
answer to an incapable terminal is to **tell the truth**, not to burn a second
chord family.

**D2 — Do not push the kitty keyboard protocol.**
`run.rs:466-478` documents a concrete regression (ghostty's event-type CSI-u
spilling literal characters into the focused pane on termwiz 0.23.3). Nothing in
THE-70 changes that calculus.

**D3 — Probe the keyboard, don't guess it.**
Extend the _existing_ startup probe rather than adding a new I/O path or a
terminal allowlist. The probe already runs after `set_raw_mode()` (so
`modifyOtherKeys=2` is already pushed) and before termwiz takes the tty —
querying `XTQMODKEYS` there returns _the level actually in effect_, which is a
direct confirmation that our push took, not an inference from `TERM`.

**D4 — Never hide a hint we can't prove is broken.**
Suppression is conservative and one-directional: hints disappear only on
`Some(false)` (proved undeliverable). No probe, no reply, non-tty, Windows,
`THEGN_PROBE_MS=0`, every test → `None` → today's behaviour, byte for byte.

**D5 — Keep app-tab Alt+`<digit>` switching; make it narrow and honest.**
Removing it is a product call, not an architect's. Fix the two unambiguous
defects (exact-modifier match; guard `]`/`[`), and make the sidebar stop
painting worktree digits for the slots the app-tab switcher actually claims, so
the hint and the dispatch agree. If the owner later wants the collision gone,
the clean follow-up is to give app-tab switching real `ACTION_SPECS` actions
instead of a pre-keymap intercept — noted, not done here.

**D6 — Pane relay: correct encoding, no protocol negotiation.**
Emit CSI-u for Ctrl+`<char>` combos that have **no legal legacy control byte**,
and ESC-prefix Alt. An unknown CSI is inert in every terminal app; today's
`0x11` is actively harmful. This needs no signature change and no `run.rs` edit.
The fuller relay — reading the pane's `TermMode::DISAMBIGUATE_ESC_CODES`
(`alacritty_terminal-0.26.0/src/term/mod.rs:75`, already tracked, and already
exposed in exactly this shape by `application_cursor()` at
`emulator.rs:595-597`) and forwarding `AlacrittyEvent::PtyWrite` so an inner
app's `CSI ? u` query gets answered (dropped today at `emulator.rs:272-290`) —
is a **documented follow-up**, not in this change.

---

## 3. Architecture / invariant compliance

- **`thegn-core` stays substrate-free.** All new keyboard-capability _parsing
  and policy_ is pure functions on `ProbeResult` in
  `thegn_core::termcaps` — no I/O, no tokio, no termwiz. Coverage-gated at 95%,
  so every branch gets a unit test.
- **New state goes on `ProbeResult`, not `TermCaps`.** `TermCaps` is the
  _render_ degradation set consumed by the `caps` chokepoint; keyboard
  reporting is an _input_ concern and belongs with the probe. Practical
  benefit: `ProbeResult` derives `Default` and is only ever built by
  `interpret_probe`/`default()`, so adding fields breaks no call site —
  whereas `TermCaps` has a struct literal at `doctor.rs:3002` that a new field
  would break, coupling otherwise-independent chunks.
- **0% idle.** No new thread, no new wake source, no new timer. The probe is
  the same single bounded read that already happens once at startup; only the
  query string and the parser change.
- **Sub-300 ms launch.** No extra round trip: the added queries ride in the same
  `write_all` before the DA terminator, inside the existing `PROBE_BUDGET`.
- **Render decision stays pure.** `render_plan::plan` is untouched. The new
  signal reaches the painter as a plain `FrameModel` field, so `build_sidebar`
  stays a pure function of the model and is unit-testable — no new global read
  at a draw site.
- **Degrade at the edges.** Hint suppression is a caps-driven degradation
  decided once and applied at the composition site, matching
  `caps::active_glyphs()`'s shape.
- **Ratchets.** No new platform `#[cfg]` outside `platform/`, no color/glyph
  literals at draw sites, no `gh` calls, no `async fn` in a provider trait, no
  new ignored `Result`s beyond the existing best-effort write pattern (comment
  required). The **help ratchet** is the one to watch: no new `ACTION_SPECS`
  ids are added, so no `docs/help/` `actions:` frontmatter changes are needed —
  but the prose ratchet requires that any page which _claims_ an id actually
  mentions it, so edits to `docs/help/*.md` must not delete an existing chord
  mention.
- **`just term-check`** greps only the `color` / `glyphs` rows of
  `Resolved capabilities` (`justfile:826-829`), so a new `doctor` row is safe.

---

## 4. Risks the coders must respect

1. **`CSI ? 4 m` (XTQMODKEYS) has a private-parameter marker.** A conformant
   parser ignores an unrecognised private CSI, but a sloppy one could read it as
   `SGR 4` (underline). Mitigation is mandatory: append a plain `\x1b[m` after
   the query batch.
2. **The probe's early-break heuristic is fragile** (`probe.rs:134-138`: first
   `?` then any `c`). A terminal whose XTVERSION name contains a `c` can break
   the read early — a _pre-existing_ hazard, since XTVERSION already precedes
   DA. Do not make it worse: put the new queries **before** `\x1b[>q\x1b[c`, and
   cover partial buffers with `interpret_probe` unit tests that degrade to
   "unknown" rather than to a wrong answer.
3. **Never let `None` (unknown) suppress anything.** A regression here silently
   deletes a working affordance for every user whose terminal is merely quiet.
4. **`model.app_tabs` must be populated before the first frame** (set at
   `run.rs:11811` / `:11828`). If it can be empty on the first paint, treat
   `len() <= 1` as "nothing claimed" — which is also the correct default.
5. **ESC-prefixing Alt in `key_bytes` is a behaviour change for panes.** It is
   the standard legacy meta encoding and the right fix, but it does reach real
   child processes: an inner app that ignores `ESC`+char sees a stray ESC where
   it previously saw a bare character. Call it out in the commit body.

---

## 5. Chunk map

| chunk | scope                                                     | crate        | depends on  | parallel with |
| ----- | --------------------------------------------------------- | ------------ | ----------- | ------------- |
| 1     | Keyboard-reporting probe parsing + policy (pure)          | `thegn-core` | —           | **2**         |
| 2     | Pre-keymap dispatch defects + pane key encoding           | `thegn-host` | —           | **1**         |
| 3     | Probe wiring, honest hints, `doctor`, panic restore, docs | `thegn-host` | **1 and 2** | —             |

Chunks 1 and 2 are file-disjoint and have no code dependency on each other —
run them **in parallel**. Chunk 3 touches `run.rs` (as does chunk 2) and
consumes chunk 1's API, so it runs **after both**.

Chunk files: `.thegn/pipeline/THE-70/code/chunk-1.md`, `chunk-2.md`, `chunk-3.md`.

## 6. Explicitly out of scope

- Pushing the kitty keyboard protocol (D2).
- Remapping the workspace summon family (D1).
- Moving app-tab switching into `ACTION_SPECS` (D5 follow-up).
- Emulator-side keyboard-mode relay + `PtyWrite` reply forwarding (D6 follow-up).
- `parse_chord` letter-case semantics — settled and documented on main (§1.8).
