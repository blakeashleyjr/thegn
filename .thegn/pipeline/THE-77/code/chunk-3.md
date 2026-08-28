# THE-77 chunk 3 — Empty `test/help-context-ratchet.txt` by documenting the two reserved panel sections

**Finding:** F6 of `.thegn/pipeline/THE-77/architect/design.md`.

## Files touched (exact)

- `docs/help/panel.md`
- `crates/thegn-host/src/help/pages.rs`
- `test/help-context-ratchet.txt`

## Overlap / dependency

**None with chunk 1 or chunk 2 — fully file-disjoint, runs in parallel with
both.** This chunk touches no `test/*-ratchet.txt` other than
`help-context-ratchet.txt`, and neither `test/ratchet.sh` nor the `justfile`.

---

## Background

`test/help-context-ratchet.txt` is two entries from zero: `panel:db` and
`panel:debug`. Per that file's header, an unclaimed context means pressing `F1`
in that section falls back to the generic index page and teaches you nothing.

Both sections are **inert placeholders**, not unimplemented features:

- `crates/thegn-host/src/panel/sections/misc.rs:1160` (`db()`) renders
  `"○ no database detected"` / `"db introspection not wired yet"`.
- `crates/thegn-host/src/panel/sections/misc.rs:784` (`debug()`) renders
  `"○ no session"`, a `BREAKPOINTS` header with `"none set"`, and
  `"debugger integration not wired yet"`.

Both sit outside `SECTION_ORDER` — `help::context::vocabulary()`
(`crates/thegn-host/src/help/context.rs:50-55`) chains them in explicitly as
"the two outside SECTION_ORDER" — so they are reachable but not in the normal
tab rotation. Both belong to the **system** tab (`panel/mod.rs:289`), and their
key strings are `"db"` and `"debug"` (`panel/mod.rs:244`).

**Do not document them as working features.** The honest move is the one
ARCHITECTURE.md §6 already makes for routed-but-inert capability rows: mark them
as reserved so they never read as done.

---

## Approach

### 1. `docs/help/panel.md` — claim both contexts and write the prose

Add `panel:db` and `panel:debug` to the `contexts:` array in the frontmatter
(currently `docs/help/panel.md:5-19`). Keep the list's existing formatting —
`treefmt` runs on this file in pre-commit and will reflow it; let it.

Add a short **Reserved sections** subsection to the body. It must state plainly
that these two are placeholders: present in the system tab, rendering a
"not wired yet" line, with no behaviour behind them yet. Two or three sentences.
Match the page's existing voice — read the rest of `panel.md` first; it is
terse, second-person, and uses backticked key chords.

Note also that the "system" bullet in the tab list near the top of the page
(`docs/help/panel.md:32-34`) enumerates the system sections and omits `db` and
`debug`. Decide whether to add them there too — if you do, make it visibly clear
they are reserved rather than silently padding the list.

### 2. `crates/thegn-host/src/help/pages.rs` — update the pinned assertion

`context_pages_resolve` (`crates/thegn-host/src/help/pages.rs:188-201`) asserts:

```rust
// A context nobody claims lands on index, never nowhere. (`panel:debug`
// is a dev-only section — see test/help-context-ratchet.txt.)
assert_eq!(reg.page_for_context("panel:debug"), Some("index"));
```

Once `panel.md` claims it, this resolves to `Some("panel")`. Update the assertion
**and its comment** — the comment currently points at the ratchet file that this
chunk empties, so leaving it would be a stale cross-reference. The property the
line is really guarding ("an unclaimed context lands on index, never nowhere") is
still worth asserting; if there is another genuinely unclaimed context key to
assert it with, use that, otherwise fold the check into the claimed-context
assertions above it and say why in the comment.

### 3. `test/help-context-ratchet.txt` — delete both entries

Delete the `panel:db` and `panel:debug` lines. **Keep the header comment block**
— the ratchet test still runs and still refuses new panel sections without a
help page; the file is now an empty allowlist, which is the terminal state the
header describes.

`every_panel_context_has_a_documentation_page`
(`crates/thegn-host/src/help/ratchet_tests.rs:121`) asserts in both directions:
it fails if a context is claimed _and_ still allowlisted (`:164-166`), so steps
1 and 3 must land together.

## Tests to run (scoped — no full-workspace gates)

```sh
cargo nextest run -p thegn-host help
just quick thegn-host
```

`cargo nextest run -p thegn-host help` covers `help::ratchet_tests`
(the context ratchet, the action ratchet and the prose ratchet) and
`help::pages` (`context_pages_resolve`, `every_help_page_is_registered`). All
must pass.

Watch for the **prose ratchet** (`test/help-prose-ratchet.txt`, currently empty
and expected to stay empty): it applies to `actions:` claims, not `contexts:`, so
this chunk should not affect it — but if it fires, you have added an action
claim by accident.

Do not run `just test`, `just lint`, `just ci`, `just coverage`, or e2e. Do not
run `just help-ratchet-update` — the ratchet files are edited by hand here and
regenerating would also rewrite the action allowlists.

## Done criteria

- `docs/help/panel.md` frontmatter claims `panel:db` and `panel:debug`, and the
  body describes both as reserved placeholders — accurately, matching what
  `misc.rs:784` / `misc.rs:1160` actually render.
- `test/help-context-ratchet.txt` has zero non-comment lines; its header block is
  intact.
- `crates/thegn-host/src/help/pages.rs`'s `context_pages_resolve` passes and its
  comment no longer points at a now-empty allowlist as if it held entries.
- `cargo nextest run -p thegn-host help` passes.
- `just quick thegn-host` clean.
- `git status` shows exactly the three files listed at the top.

**Exact commit subject (use verbatim):**

```
docs(the-77): document the reserved db/debug panel sections, emptying the help-context ratchet
```
