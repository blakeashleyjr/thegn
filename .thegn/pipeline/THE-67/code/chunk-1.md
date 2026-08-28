# Chunk 1 — Pure hit geometry: the drag-band module + the dead tab-chip gap

**Issue:** THE-67 (drag/drop needs unnecessary mouse precision).
**Design:** `.thegn/pipeline/THE-67/architect/design.md` — read §1 (the drag
model), §2.2, §2.4 and §3.1/§3.4 before starting.
**Runs:** in **parallel with chunk 3**. **Chunk 2 depends on this chunk's
API** — the `drag_hit` functions must exist and be named exactly as specified
below, because chunk 2 calls them from `run.rs`.

## Why

Two independent pieces of pure geometry, both unit-testable without a terminal:

1. The sidebar and panel width drags grab on a **single column** — `mx` compared
   for equality against the separator (`run.rs:12623`, `run.rs:12639`; the
   separator is one column by construction, `layout.rs:554,562`). This chunk
   builds the pure band/offset helpers; chunk 2 wires them into the loop.
2. The center tab strip lays chips out with a 1-column spacing gap
   (`chrome.rs:864`, `x += w + 1`) that `center_tab_hit` (`chrome.rs:871-876`)
   does not claim, so one column in every ~5 on the tab strip is click-dead and
   the enclosing branch (`run.rs:13038`) swallows the click.

## Files you own

- `crates/thegn-host/src/drag_hit.rs` **(new)**
- `crates/thegn-host/src/main.rs` (one `mod` line)
- `crates/thegn-host/src/chrome.rs`
- `crates/thegn-host/src/chrome_tests.rs`

Do not touch `run.rs`, `pane_drag.rs`, `sidebar_view.rs`,
`handlers/sidebar_mouse.rs`, `docs/`, or `openspec/` — chunks 2 and 3 own those.

## Approach

### 1. `crates/thegn-host/src/drag_hit.rs` (new)

A small, substrate-free module: no `Rect`, no model, no I/O — just columns.
Module doc should state that it is the shared pure geometry for the compositor's
mouse-drag grab bands, and that the band is deliberately widened into chrome
furniture only (design §3.1).

```rust
/// Which separator a grab band belongs to — the band always takes its extra
/// cell from the CENTER column's outer frame cell, never from the list beside
/// it (a sidebar row / panel row is a live click target across its full width).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SepSide {
    /// The sidebar|center separator: band is `{sep, sep + 1}`.
    Sidebar,
    /// The center|panel separator: band is `{sep - 1, sep}`.
    Panel,
}

/// Whether pointer column `mx` grabs the separator at `sep`.
pub fn sep_grab(sep: Option<usize>, side: SepSide, mx: usize) -> bool;

/// Whether `mx` is the separator column ITSELF (as opposed to the band's extra
/// furniture cell). Callers gate the extra cell on it not being pane/drawer
/// content; the separator column always grabs.
pub fn sep_is_exact(sep: Option<usize>, mx: usize) -> bool;

/// The separator column implied by pointer column `mx`, for a drag that pressed
/// at `press_x` while the separator sat at `sep`. The press offset is held for
/// the whole drag, so the divider tracks the cursor instead of jumping to it on
/// the first sample. Saturating: never underflows at column 0.
pub fn sep_follow(press_x: usize, sep: usize, mx: usize) -> usize;
```

`sep_follow` is `mx - (press_x - sep)` when `press_x >= sep`, else
`mx + (sep - press_x)`, saturating on the subtraction.

Keep it that small. Do **not** add a `moved`/threshold type here — chunk 2
tracks that with a bool beside its existing loop state, and a shared type would
force a second file into chunk 2's set.

Unit tests in the module (`#[cfg(test)] mod tests`), covering:

- `sep_grab(Some(40), Sidebar, 40|41) == true`; `39` and `42` false.
- `sep_grab(Some(40), Panel, 39|40) == true`; `38` and `41` false.
- `sep_grab(None, _, _) == false` for both sides.
- `sep_is_exact` true only on the separator column, false on the extra cell and
  on `None`.
- **The degenerate 1-column center**: with `sep_left = 40` and `sep_right = 42`
  (a center of exactly one column, `layout.rs:564-565`), column 41 is in **both**
  bands. Assert both return `true` and note in the test that the caller resolves
  it by checking the sidebar first (`run.rs:12623` precedes `:12636`).
- `sep_follow`: pressing on the separator is identity — `sep_follow(40, 40, 55)`
  is `55`; pressing one cell right of it keeps the 1-cell offset —
  `sep_follow(41, 40, 55)` is `54`; pressing one cell left keeps it the other
  way — `sep_follow(39, 40, 55)` is `56`; and it saturates at `mx = 0`.

### 2. `crates/thegn-host/src/main.rs`

Add `mod drag_hit;` in the alphabetical run — `_` sorts before `d`, so it goes
**immediately before** `mod dragdrop;` (currently `main.rs:50`).

### 3. `crates/thegn-host/src/chrome.rs` — the tab-chip gap

`strip_chip_spans` (`chrome.rs:839-867`) stays **exactly as it is**: it is the
one source of chip PLACEMENT, shared with `draw_center_tabs`, and widening the
painted spans would move the paint. The widening belongs to the hit test only.

In `center_tab_hit` (`chrome.rs:871-876`), a chip's hit span becomes
`[sx, sx + w + 1)` — it absorbs the single spacing column drawn after it —
clamped so it never crosses the boundary at which the chips stop. That boundary
is the `end` local inside `strip_chip_spans` (`chrome.rs:844-846`:
`pin_chips_start(model, strip) - env_cluster_width(model)`, floored at
`strip.x`). Factor it into a private helper (e.g. `strip_chip_end(model, strip)
-> usize`) used by both `strip_chip_spans` and `center_tab_hit`, rather than
recomputing it inline in two places.

Properties that must hold, and that your tests must pin:

- Spans stay non-overlapping — the next chip starts at `sx + w + 1`, so the gap
  column resolves to the chip on its **left**, never ambiguously.
- A column at or past `strip_chip_end` never resolves to a tab (the pin strip
  and env cluster keep their cells; `pin_chip_hit` is unchanged).
- Columns **before** the first chip stay `None`. Two pre-existing assertions
  must keep passing **unchanged** — the widening is to the right only:
  `chrome_tests.rs:1429-1433` (column 11 is `None`, the "old (wrong) column"
  guard for wide leaves) and `chrome_tests.rs:1384` (column 0 is `None`).

Update the doc comment on `center_tab_hit` to say the hit span includes the
chip's trailing spacing column, and why (no dead cells between chips).

### 4. `crates/thegn-host/src/chrome_tests.rs`

Add a test — `center_tab_hit_claims_the_gap_between_chips` or similar — that
builds a model with two tabs, takes `strip_chip_spans`, and asserts:

- the column `spans[0].0 + spans[0].1` (the gap, previously dead) now returns
  `Some(0)`;
- `spans[1].0` still returns `Some(1)`, i.e. the widening did not swallow the
  next chip;
- with pins present (mirror the `center_tabs_render_pin_chips_right_aligned`
  fixture at `chrome_tests.rs:1460`), the last tab's widened span does not reach
  into the pin strip: the column where `pin_chip_hit` returns `Some(..)` must
  still return `None` from `center_tab_hit`.

## Tests to run (scoped — no full-workspace gates)

```sh
just quick thegn-host
cargo nextest run -p thegn-host drag_hit
cargo nextest run -p thegn-host center_tab
cargo nextest run -p thegn-host pin_chip
```

Do **not** run `just test`, `just ci`, `just coverage`, or `just e2e`.

## Done criteria

- `crates/thegn-host/src/drag_hit.rs` exists, is declared in `main.rs`, and
  exports `SepSide`, `sep_grab`, `sep_is_exact`, `sep_follow` with the exact
  names and signatures above (chunk 2 calls them verbatim).
- Every listed `drag_hit` unit test passes, including the 1-column-center
  overlap case.
- `center_tab_hit` claims the inter-chip gap column; the two pre-existing
  `None` assertions in `chrome_tests.rs` still pass untouched.
- `just quick thegn-host` is clean (clippy `-D warnings`).
- No `#[allow(dead_code)]` added: `drag_hit`'s functions are exercised by its own
  tests in this chunk and by `run.rs` in chunk 2. If clippy flags an unused
  function in this chunk, that is expected only for functions your tests do not
  call — make the tests call all four rather than silencing the lint.

**Commit subject (exact):**

```
feat(the-67): pure drag grab-band geometry + tab-chip gap hit
```
