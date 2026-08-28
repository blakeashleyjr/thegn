# THE-70 chunk 3 — wire the probe, make the digit hints honest, report it (`thegn-host` + docs)

Read `.thegn/pipeline/THE-70/architect/design.md` in full before starting —
especially §1.4, §1.7, §2 (D1/D3/D4/D5), §3 and **all five risks in §4**.

## Dependency — this chunk runs LAST

- **Requires chunk 1** (`thegn_core::termcaps::{ProbeResult::modify_other_keys,
ProbeResult::kitty_keyboard, ProbeResult::ctrl_digit_reportable,
KEYBOARD_QUERIES}`). Verify those exist before starting; if they don't, the
  chunk-1 coder is still running.
- **Requires chunk 2**, which edits a different region of `run.rs`. Both must be
  on the branch before you start, or you will conflict.

## Files touched (exact, exhaustive)

- `crates/thegn-host/src/probe.rs` — add the keyboard queries to the batch.
- `crates/thegn-host/src/run.rs` — three small, separate edits:
  - the startup probe log at `run.rs:513-520` (add the keyboard fields),
  - the model wiring where `model.app_tabs` is set (`run.rs:11811` / `:11828`)
    or wherever the frame model is first built — set the new field once,
  - the normal-teardown restore string at `run.rs:1073`.
- `crates/thegn-host/src/platform/unix.rs` — the panic-restore `SEQ`
  (`platform/unix.rs:58`).
- `crates/thegn-host/src/chrome.rs` — one new `FrameModel` field.
- `crates/thegn-host/src/sidebar_view.rs` — the slot assignment
  (`sidebar_view.rs:558-578`) plus its tests.
- `crates/thegn-host/src/cmd/doctor.rs` — a new resolved-capabilities row and
  `--json` key.
- `docs/help/terminal-compatibility.md`, `docs/help/sidebar.md`,
  `docs/help/getting-started.md`, `docs/help/workspaces-and-worktrees.md`,
  `docs/help/drawer-and-corner.md`.

## Approach

### 1. Ask the keyboard questions (`probe.rs:110-116`)

Today:

```rust
out.write_all(b"\x1b[>q\x1b[c").ok()?;
```

Prepend chunk 1's constant so the keyboard replies arrive **before** the Primary
DA reply, which is the batch terminator the read loop breaks on
(`probe.rs:133-139`):

```rust
out.write_all(thegn_core::termcaps::KEYBOARD_QUERIES).ok()?;
out.write_all(b"\x1b[>q\x1b[c").ok()?;
```

(or one combined write — either is fine, ordering is what matters).

Update the module doc (`probe.rs:1-23`) and the inline comment at
`probe.rs:110-112` to name the four queries and why the DA stays last.

**Risk §4.1 is already handled** by `KEYBOARD_QUERIES`' trailing `\x1b[m`; do
not drop it. **Risk §4.2:** leave the early-break heuristic
(`probe.rs:134-138`) alone — it is a pre-existing fragility and changing it is
out of scope; just do not make it worse by putting a query after the DA.

Nothing else in `probe.rs` changes: same budget, same single bounded read, same
raw-tty gate, same `None` when skipped. **No new I/O path, no new thread, no
extra round trip** — the 0%-idle and sub-300 ms launch invariants are untouched.

### 2. Thread the answer to the frame model

- `chrome.rs` (`FrameModel`, near the existing `sidebar_focused` / `app_tabs`
  fields at `chrome.rs:644`): add

  ```rust
  /// Whether the outer terminal can report `Ctrl+<digit>` distinctly, from
  /// the startup probe. `None` = unknown (no probe / silent terminal) —
  /// which MUST be treated as "assume it works". See THE-70.
  pub ctrl_digits_reportable: Option<bool>,
  ```

  `Option<bool>` defaults to `None`, so `FrameModel::default()` and every test
  keep today's behaviour with no edit.

- `run.rs`: set it once from `term_probe` (already in scope at `run.rs:512`) —
  `model.ctrl_digits_reportable = term_probe.as_ref().and_then(|p| p.ctrl_digit_reportable());`
  — at the same place the model is first populated. Do not recompute it per
  frame; it cannot change during a session.

- `run.rs:513-520`: add `modify_other_keys` / `kitty_keyboard` to the existing
  `thegn::startup` "outer-terminal probe" event so `THEGN_LOG=info` shows them
  in the waterfall.

Deliberately **not** put on `TermCaps` / the `caps` global: `TermCaps` is the
render degradation set, this is an input concern, and a new `TermCaps` field
would break the struct literal at `doctor.rs:3002`. Keeping it on `FrameModel`
also keeps `build_sidebar` a pure function of the model — no new global read at
a draw site, so the render-decision purity invariant holds and the painter stays
unit-testable.

### 3. Make the sidebar digit hints honest (`sidebar_view.rs:558-578`)

The slot assignment currently numbers every switchable workspace row 1..9 and
every Tab-target worktree row 1..9 whenever the sidebar is focused. Two
suppressions, both **one-directional — only ever hide a digit we can prove does
not work**:

**3a — workspace (Ctrl) digits.** When
`model.ctrl_digits_reportable == Some(false)`, assign `None` to every workspace
slot. `Some(true)` and `None` behave exactly as today.

**3b — worktree (Alt) digits claimed by app tabs.** Chunk 2 narrowed the
app-tab intercept but kept the Alt+1..N collision by design (§2 D5). Compute
`claimed = if model.app_tabs.len() > 1 { model.app_tabs.len() } else { 0 }` and
assign `None` to worktree slots `<= claimed`. Dispatch is unchanged — `Alt+3`
still means worktree 3 — so numbering does not shift; the lower digits simply
stop being advertised because the app-tab switcher takes them first.

**Risk §4.4:** if `model.app_tabs` can be empty on the first frame,
`len() <= 1` already yields `claimed == 0`, which is the correct default. Verify
rather than assume.

Update the comment at `sidebar_view.rs:554-559` to state both rules. The
painters at `sidebar_view.rs:1388-1395` (workspace) and `:1502-1506` (worktree)
already reserve the 3-column gutter when `slot` is `None`, so **layout does not
shift** and no e2e baseline should move. Do not touch them.

### 4. Restore `modifyOtherKeys` on the panic path (`platform/unix.rs:58`)

The normal teardown is covered — `run.rs:1082` calls `set_cooked_mode()`, which
termwiz implements as `modify_other_keys(1)`. The panic restore is not: it
writes a raw sequence then `tcsetattr`, leaving the user's shell in
`modifyOtherKeys = 2` where readline sees CSI-u it cannot parse.

Add `\x1b[>4m` (XTMODKEYS with the value omitted = reset the resource to its
initial value) to the `SEQ` at `platform/unix.rs:58`, and to the normal-path
string at `run.rs:1073` for symmetry. Update both comments.

While you are in those two comments: both strings pop the kitty keyboard stack
with `\x1b[<u` although thegn never pushes it. **Leave the bytes alone** — a pop
of an empty stack is a documented no-op and it is defensive against an inner app
that leaked flags — but add a one-line note saying exactly that, so the next
reader does not "fix" it.

### 5. Report it in `thegn doctor`

`doctor.rs` already has the probe in hand (`doctor.rs:994-997`,
`doctor.rs:1173-1179`). Add to the **Resolved capabilities** block a row:

```
  keyboard    modifyOtherKeys=2 (Ctrl+<digit> chords OK)
  keyboard    not reported (Ctrl+1..9 / Ctrl+Alt+1..9 cannot reach thegn)
  keyboard    unknown (no probe — assuming supported)
```

and the same three states as a `--json` key. When the state is `Some(false)`,
print an actionable remedy near it, e.g.:

- inside tmux: `set -g extended-keys on` (plus `set -as terminal-features
'*:extkeys'` on tmux 3.4+);
- otherwise: use a terminal that supports xterm `modifyOtherKeys` level 2, or
  rebind the family — `[keymap]` accepts `summon-workspace-1` … `-9`
  (`keymap.rs:766-790`), e.g. `"Alt Shift 1" = "summon-workspace-1"`.

`just term-check` only greps the `color` and `glyphs` rows
(`justfile:826-829`), so a new row is safe — but keep the existing rows' text
and order untouched.

### 6. Docs

- **`docs/help/terminal-compatibility.md`** — the substantive addition. A new
  `## Keyboard` section: what `modifyOtherKeys` is, that thegn pushes level 2 at
  startup and deliberately does not push the kitty protocol (cite the reason
  from `run.rs:466-478` in prose), which chords depend on it
  (`Ctrl+1..9` workspace jump, `Ctrl+Alt+1..9` pins), what happens without it
  (the §1.4 table is worth reproducing — Ctrl+2 lands on the palette, Ctrl+3
  reads as Escape), how `thegn doctor` reports it, and the tmux / rebind
  remedies.
- **`docs/help/sidebar.md:36`, `docs/help/getting-started.md:30`,
  `docs/help/workspaces-and-worktrees.md:51`,
  `docs/help/drawer-and-corner.md:59`** — each currently states the digit
  chords flatly. Add a one-line caveat pointing at
  `terminal-compatibility.md`. **Help-ratchet rule: do not delete an existing
  chord mention** — the prose ratchet requires a page that claims an action id
  in its `actions:` frontmatter to actually mention that chord/id/label. You are
  adding no new `ACTION_SPECS` ids, so no frontmatter changes are needed and
  `test/help-*-ratchet.txt` must not need regenerating. If the help ratchet
  fails, you removed a mention — restore it rather than editing the allowlist.

## Tests

- **`sidebar_view.rs` tests** (the important ones — pure over `FrameModel`):
  - `ctrl_digits_reportable: Some(false)` ⇒ no workspace row carries a slot;
    worktree slots are unaffected.
  - `Some(true)` and `None` ⇒ workspace slots identical to today (assert
    against the existing expected numbering — this is the D4 guard).
  - `app_tabs.len() == 3` ⇒ worktree slots 1 and 2 and 3 are `None`, slot 4+
    still numbered 4.. (numbering does not shift).
  - `app_tabs.len() <= 1` ⇒ worktree slots unchanged from today.
  - sidebar not focused ⇒ all slots `None`, as before.
  - A row-height / column-position assertion showing the gutter is still
    reserved when a slot is suppressed (no layout shift).
- **`doctor.rs` tests**: the three `keyboard` states render distinct strings;
  `--json` carries the state.
- **`probe.rs`**: no unit test (it is an I/O seam excluded from the coverage
  gate). If it is cheap, assert `KEYBOARD_QUERIES` is written before `\x1b[c`
  by construction — otherwise skip.

## Tests to run (scoped — do NOT run a full-workspace gate)

```sh
just quick thegn-host
cargo nextest run -p thegn-host sidebar_view
cargo nextest run -p thegn-host doctor
cargo nextest run -p thegn-host help::ratchet
```

Do **not** run `just test`, `just ci`, `just coverage`, or `just e2e` in the
headless coding turn. The pre-push hook is the heavy gate.

If you have a real terminal available, a two-minute manual check is worth more
than any of the above: `thegn doctor` and read the new `keyboard` row.

## Done criteria

- The startup probe asks the two keyboard questions, the answers reach
  `FrameModel`, and `THEGN_LOG=info`'s startup waterfall shows them.
- With `Some(false)`, the sidebar paints no workspace digits; with `Some(true)`
  or `None`, the sidebar is byte-identical to before this change.
- Worktree digits claimed by app tabs are not advertised; numbering does not
  shift; the gutter is still reserved (no layout shift, no e2e baseline churn).
- `thegn doctor` prints a `keyboard` row in all three states with an actionable
  remedy when broken, and exposes it in `--json`.
- Both restore paths reset `modifyOtherKeys`; the `\x1b[<u` pop is documented
  rather than removed.
- Help pages explain the limitation and the rebind escape hatch; the help
  ratchets pass **without** regenerating any allowlist.
- `just quick thegn-host` clean.

**Commit subject (exact):**

```
fix(the-70): surface when Ctrl+<digit> summon can't reach thegn
```

In the commit body: state that Ctrl+`<digit>` is undeliverable without
`modifyOtherKeys` level 2 (and misfires — Ctrl+2 → palette, Ctrl+3 → Escape),
that thegn now probes for it instead of assuming, that unknown always means
"assume it works", and that the workspace family was deliberately **not**
remapped (design D1 — it is rebindable via `[keymap]`).
