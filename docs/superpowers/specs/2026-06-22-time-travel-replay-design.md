# Time-travel replay, search-across-time, registers & screen-swap — design

Date: 2026-06-22
Status: proposed

Inspired by [`cy`](https://cy.cyber.systems)'s replay mode. cy's defining
feature is that it records the _full event stream_ of every pane and lets you
scrub it like a video player — pause, play forwards/backwards, skip idle gaps,
and search for any string that **ever appeared on screen**, including inside
full-screen apps (vim/htop) where output never reaches the scrollback. This doc
architects that capability onto superzej's existing seams, plus three smaller
cy borrowings (search-across-time, vim-style registers, alt/main screen swap).

Ordered by value. Phases 1–2 are one feature and the reason to do any of this;
3–4 are independent polish that can land in any order.

## What we already have (don't rebuild)

- **Copy mode** — `copymode.rs`: a pure `Selection` model over the grid,
  `extract()`, and `osc52()`. Mouse-drag + keyboard-cursor builds the selection
  in `run.rs`.
- **Clipboard** — `clipboard.rs`: OSC 52 to the outer terminal _and_ native CLI
  fallback (`wl-copy`/`xclip`/`pbcopy`). Already more robust than cy's.
- **Scrollback** — `emulator.rs`: vt100 history ring (10 000 lines), viewport
  scroll via `scroll_up/down/reset`.
- **History search** — `search.rs` + `superzej-core::search`: `SearchOverlay`
  modal, nucleo over the per-pane ANSI-stripped `HistoryBuffer`, scoped
  Pane→Tab→Worktree→Workspace→Profile.

The gap cy fills: all of the above sees only the **current** grid + the
line-flushed scrollback. None of it can answer "what did this pane show 5
minutes ago" or "find that error that scrolled past inside `nvim`".

## The one tee point everything hangs off

Every byte a pane emits already funnels through a single method:

```rust
// pane.rs:317
pub fn feed(&mut self, bytes: &[u8]) {
    self.emulator.advance(bytes);                  // styled grid
    feed_bytes_to_history(bytes, &mut self.history, …);  // ANSI-stripped ring
}
```

`feed` is called from exactly one place — the `PaneEvent::Output` drain in the
event loop. **This is the recording tap.** Recording is a third sink alongside
the emulator and the history ring; nothing else in the codebase needs to know.

---

## Phase 1 — the recorder (substrate for everything)

### 1.1 Data model: keyframes + byte deltas

A pane's history is a byte stream with timestamps. Storing raw bytes is the
most faithful and compact representation (it _is_ what the emulator consumed),
but seeking to time _T_ by replaying from byte 0 is O(history). cy solves this;
we use the standard keyframe approach:

```rust
// crates/superzej-host/src/replay.rs  (new)

/// One recorded pane session. Lives beside the PtyPane.
pub struct Recording {
    /// Monotonic origin; all event offsets are ms since this.
    epoch: std::time::Instant,
    /// Periodic full-emulator snapshots for O(1)-ish seeking.
    keyframes: Vec<Keyframe>,
    /// Raw byte deltas between keyframes, each stamped with an offset.
    events: ringbuf::Ring<Event>,
    /// Rolling byte + duration budget (see 1.3).
    budget: Budget,
}

struct Event { at_ms: u64, bytes: Box<[u8]> }

struct Keyframe {
    at_ms: u64,
    event_idx: usize,        // events[event_idx] is the first AFTER this frame
    grid: GridSnapshot,      // serialized emulator state (see 1.2)
    cursor: (u16, u16),
    rows: u16, cols: u16,
}
```

Recording mirrors `feed`: on each `Output` chunk, push an `Event { at_ms,
bytes }`. Every `KEYFRAME_INTERVAL` (default: 4 s of activity **or** 256 KiB of
bytes, whichever first) capture a `Keyframe` from the live emulator. Resize is
recorded as a synthetic event so replay re-`resize()`s at the right moment.

### 1.2 GridSnapshot — serialize the emulator, don't fork it

Replay needs to reconstruct emulator state at a keyframe. Two options:

- **(a) Re-feed from the previous keyframe's bytes.** Requires no new emulator
  API. A keyframe is then just `(at_ms, event_idx)` — a _marker_, not a state
  dump. Seeking to _T_: binary-search the marker ≤ _T_, find the marker before
  _that_ (so we have a clean starting grid), spin up a fresh `Vt100Emulator`,
  re-feed bytes from there to _T_. Keyframes cap replay cost at one interval's
  worth of bytes.
- **(b) Serialize/restore grid state.** Needs a new `PaneEmulator` method.

**Decision: (a).** It needs _zero_ emulator-trait changes, is exact by
construction (the same bytes through the same parser), and the cost ceiling is
one keyframe interval (≤256 KiB) — milliseconds. The keyframe is purely an
index into the byte log. This keeps the `PaneEmulator` seam clean, which the
trait doc explicitly protects (the high-fidelity emulator swap must stay
drop-in).

So `Keyframe` collapses to `{ at_ms, event_idx, rows, cols }` and
`GridSnapshot` is unnecessary. Replay = "fresh emulator + re-feed a bounded
byte slice." Elegant and it falls out of the existing trait.

### 1.3 Bounded by bytes and time (perf invariant)

Unbounded recording violates the ~0%-idle / bounded-memory ethos. The ring is
capped two ways, configurable:

```toml
[replay]
enabled = true            # master switch (free when off — no allocation)
max_bytes_per_pane = "8MiB"
max_duration_per_pane = "30m"
persist = false           # write to disk for cross-restart scrubbing (Phase 1.5)
keyframe_interval_ms = 4000
keyframe_interval_bytes = 262144
```

Eviction drops whole events from the front; when an event is evicted past a
keyframe, that keyframe is dropped too (its byte range no longer exists).
8 MiB/pane is generous — a busy build log is ~tens of KiB/s; most panes idle.

Cost when `enabled = false`: the `Option<Recording>` is `None`, `feed` does one
null check. Zero allocation, matches the "instrumentation is free when off"
pattern from `SUPERZEJ_LOG`.

### 1.4 Where it lives & the event-loop contract

`PtyPane` gains `record: Option<Recording>`. `feed` appends after advancing:

```rust
pub fn feed(&mut self, bytes: &[u8]) {
    self.emulator.advance(bytes);
    feed_bytes_to_history(bytes, …);
    if let Some(r) = &mut self.record {
        r.push(bytes, /* now */ Instant::now());   // O(1) amortized; keyframe is cheap
    }
}
```

`Instant::now()` on the loop is fine — it's a vDSO read, nanoseconds, no
syscall. **Recording adds no wakeups and no blocking I/O**, so the
zero-idle-CPU invariant holds. (Disk persistence in 1.5 is the only I/O and it
goes off-thread.)

### 1.5 Optional disk persistence (off-loop)

When `persist = true`, each pane's ring is mirrored to
`$XDG_STATE_HOME/superzej/replay/<session_id>/<pane_id>.szr`. Writes happen on
a **dedicated writer thread** fed by an mpsc channel (the exact pattern the diff
fs-watcher uses for its ~1 s inotify registration — expensive work off the
loop, results/acks back over a channel). Format: a length-prefixed event log
with periodic keyframe markers; append-only, truncated to the byte budget on
rotation. On resurrection (`session.rs`), a pane with a matching `.szr` loads
its ring so you can scrub into the _previous_ run. This dovetails with the
existing resurrection layer (git = truth, DB = cache; replay logs are a new
cache class). Default off — it's the one feature with real disk cost.

---

## Phase 2 — replay mode UI (the cy "time mode" + copy mode)

A modal overlay, same shape as `SearchOverlay` (`Option<ReplayOverlay>` in
`run.rs`, captures all keys while open). Entered via a new `Action::EnterReplay`
(suggest **`Ctrl+b [`**-style or bind under the existing copy/scroll family —
see keybinds below). It is modal like search: see the `if let Some(ref mut ov) =
search` capture block around `run.rs:9316`; replay gets a sibling block.

### 2.1 ReplayOverlay state

```rust
pub struct ReplayOverlay {
    pane: u32,
    /// Scratch emulator the overlay paints from (NOT the live pane emulator).
    scratch: Vt100Emulator,
    cursor_ms: u64,          // current position in the recording
    state: PlayState,        // Paused | Playing { dir, speed }
    mode: ReplayMode,        // Time | Copy | Visual(Selection)
    search: Option<TimeSearch>,  // Phase 2 search (below)
}
enum PlayState { Paused, Playing { reverse: bool, speed: f32 } }
```

The overlay **renders from `scratch`**, never the live pane — the pane keeps
advancing in the background (its reader thread is untouched). Seeking to
`cursor_ms` rebuilds `scratch` per 1.2 (fresh emulator + re-feed from the
preceding keyframe). Seeks are debounced to one rebuild per frame so holding a
scrub key doesn't thrash.

### 2.2 Playback clock — the one place we bend "no timeout"

Playback is inherently time-driven, which tensions with the
"`poll_input(None)`, no tick" invariant. Resolution, consistent with the
codebase: **a clock thread that exists only while playing.** When the user hits
play, spawn (or unpause) a ticker thread that sleeps `frame_dt` (≈ 16–33 ms,
derived from speed) and pulses the `TerminalWaker` — identical mechanism to the
existing 2 s refresh-ticker thread, just a faster cadence and _only alive during
playback_. On pause/exit the thread parks. Idle (paused, or replay closed) =
zero wakeups. The invariant is "no _polling_ timeout"; an event source that
only fires while actively playing a video is an event producer, not a poll.

**Skip-inactivity (cy parity):** when advancing the clock, if the gap to the
next event exceeds `idle_threshold` (default 1 s), collapse it to a short
constant (e.g. 200 ms). Implemented in the clock advance, not the data: gaps are
visible in the raw timestamps, compressed only for playback pacing. A status
indicator shows "⏩ skipped 4m12s idle".

### 2.3 Scrubbing & time expressions

Time mode bindings (vim-ish, matching the search overlay's key conventions):

| Key         | Action                                         |
| ----------- | ---------------------------------------------- |
| `space`     | play/pause                                     |
| `←` / `→`   | step one event back/forward                    |
| `j` / `k`   | seek ±5 s                                      |
| `g` / `G`   | jump to start / live tail                      |
| `[` / `]`   | speed down/up (0.25×…8×)                       |
| `r`         | toggle reverse playback                        |
| `/` `?`     | search forward/back across time (Phase 2.4)    |
| `v`         | enter visual mode (select → register, Phase 3) |
| `s`         | swap alt/main screen (Phase 4)                 |
| `q` / `esc` | exit replay → snap pane to live tail           |

The search bar accepts cy's **time expressions** — `NdNhNmNs` (`1h30s`, `3d`) —
to jump a fixed delta in the search direction. A tiny parser
(`parse_duration(&str) -> Option<Duration>`) in `replay.rs`; if the query is
neither a valid regex nor a time expression it's treated as a literal (same
fallback cy uses, and the same "invalid regex → literal" rule already implicit
in our nucleo path).

### 2.4 Search across time (the second cy headline)

This is why recording stores **bytes, not just scrollback lines**. The current
`HistoryBuffer` only captures line-flushed main-screen output — it never sees a
frame painted by `nvim`/`htop` (alt-screen, cursor-addressed, no newlines). To
"find any string that ever appeared on screen," we search **rendered frames**.

Implementation: a time-search builds a frame index _lazily over the recording_.
For a query, walk keyframes; for each segment, re-feed to reconstruct grids at a
sampling cadence (every event that moves the cursor to a new line, or every
~250 ms of frames), extract each grid as ANSI-stripped text via the existing
`row_text` fast-path, and test the query (regex or literal) against the
flattened frame. First match ≥/≤ `cursor_ms` (depending on direction) becomes
the new `cursor_ms`. Matches are highlighted in the scratch grid.

Reuse: the `AnsiStripper` already exists; grid→text is `row_text` per row;
regex via the `regex` crate (already a transitive dep through several tools —
confirm in `Cargo.lock`, else `regex-lite`). This is **distinct from**
`SearchOverlay`, which stays the fast nucleo-over-scrollback path for the common
"find a command I ran" case. Time-search is the heavier "find anything that was
ever on screen" case, only reachable inside replay.

Frame indexing is bounded by the recording budget (8 MiB ⇒ a few thousand
frames worst case) and runs on `spawn_blocking` with results streamed back over
the palette-style channel, so a long search never blocks the loop.

---

## Phase 3 — vim-style registers

cy generalizes the clipboard into named registers (`"a`–`"z`, `"0`–`"9`, plus
`"+` = system clipboard, `""` = default), persisted to its store. superzej is
one-session/one-client so "global across clients" is moot, but
**persist-across-restart** is the real win.

### 3.1 Store

```rust
// crates/superzej-core/src/registers.rs  (new — pure, testable, coverage-gated)
pub struct Registers { map: BTreeMap<char, String> }
impl Registers {
    pub fn yank(&mut self, name: char, text: String);  // '"' = default
    pub fn get(&self, name: char) -> Option<&str>;
}
```

`'+'` is special-cased at the _host_ edge: yank-to-`+` also calls
`clipboard::copy` + emits `osc52`; paste-from-`+` reads the system clipboard
(new `clipboard::paste` candidate list — `wl-paste`/`xclip -o`/`pbpaste`).
Everything else lives in the in-memory map.

### 3.2 Persistence (DB v16)

Add to `db.rs` (bump `SCHEMA_VERSION` 15 → 16, add a `CREATE TABLE IF NOT
EXISTS` block — the established migration shape; no destructive migration
needed since it's purely additive):

```sql
CREATE TABLE IF NOT EXISTS registers (
    name       TEXT PRIMARY KEY,   -- single-char register id
    value      BLOB NOT NULL,
    updated_at INTEGER NOT NULL
);
```

Loaded into `Registers` at startup, written on yank (off-loop via the existing
DB-write path — DB writes already never sit on the loop). The volatile `"+`
register is **not** persisted (it's the live system clipboard).

### 3.3 UX

In copy/visual mode, a register prefix arms the next yank: `"` then `[a-z0-9+]`
then `y`. Paste-from-register: a new `Action::PasteRegister` prompts for the
register char then writes `get(name)` into the focused pane via
`write_input` (honoring bracketed-paste, which the emulator already reports via
`bracketed_paste()`). Without a prefix, yank/paste use `""` — i.e. today's
behavior is unchanged, registers are strictly additive.

Scope note: superzej's copy mode is mouse-drag + keyboard-cursor, not full vim
visual motions. Registers ride on top of the _existing_ selection model; we are
not importing vim's motion grammar. Modest by design.

---

## Phase 4 — alt/main screen swap in copy mode

cy lets you, mid-`htop`, swap back to the main screen's scrollback (`s`). Two
ways to get this:

- **Subsumed by replay:** scrub back to just before the alt-screen app launched.
  Free once Phase 1–2 land — but not as instant as a toggle.
- **Live toggle:** expose the retained main screen so copy mode can read it
  while the app holds the alt screen.

vt100 retains the normal screen + its scrollback while the alternate screen is
active, but the `PaneEmulator` trait exposes only the active `screen()`. A live
toggle needs a small, additive trait method:

```rust
/// View the normal (main) screen's grid even while an alt-screen app is active.
/// `None` if the emulator can't distinguish screens. Default: None.
fn alternate_active(&self) -> bool { false }
fn cell_on_screen(&self, screen: Screen, row: u16, col: u16) -> Option<GridCell> { … }
```

**Decision: ship the replay-subsumed path in Phase 2 (free), defer the live
toggle to a follow-up** pending a check of vt100 0.16's public API for reading
the inactive screen. If vt100 doesn't expose it cleanly, the live toggle is not
worth forking the emulator over — replay already answers the underlying need
("see what was there before the TUI"). Flagging the uncertainty rather than
committing to an API that may not exist.

---

## Keybinds & Action enum

Add to `keymap.rs` `Action` (host): `EnterReplay`, `PasteRegister`. Replay's
internal keys (space/j/k/g/…) are handled inside `ReplayOverlay::handle_key`,
not the global keymap — same as `SearchOverlay` owns its own keys. Suggested
default entry: extend the existing scroll/copy family; the user picks the exact
chord (the nav-ux doc shows Super+Alt is unreliable on this machine, so prefer a
plain `Alt`+letter or a `Ctrl` chord). Defer the binding choice to a quick
confirm at implementation time.

## Persistence summary

| Data                | Where                                          | Lifetime                    |
| ------------------- | ---------------------------------------------- | --------------------------- |
| Replay ring         | in-memory per `PtyPane`                        | session, bounded 8 MiB/30 m |
| Replay log (opt-in) | `replay/<session>/<pane>.szr`, off-loop writer | across restart              |
| Registers           | `registers` DB table (v16), `+` excluded       | across restart              |

## Testing

- **`replay.rs` (host unit):** record a known byte script, assert seek-to-T
  reconstructs the exact grid the live emulator showed at T (re-feed determinism
  — feeds the same `Vt100Emulator` round-trip the emulator tests already use);
  budget eviction drops oldest events + orphaned keyframes; skip-inactivity
  collapses a >1 s gap; `parse_duration` table tests (`1h30s`, `3d`, junk→None).
- **`registers.rs` (core, coverage-gated 95%):** yank/get, default register,
  overwrite, char validity. Pure — no I/O, fits the core gate.
- **time-search:** regex + literal + time-expression against a recorded
  alt-screen frame sequence (the case scrollback search _can't_ do — that's the
  whole point of the test).
- **DB v16 migration:** fresh-create has `registers`; a v15 DB upgrades without
  data loss (mirror `migrates_v5_tab_layout_into_groups`).
- **Headless PTY (`test/`):** the e2e harness already drives panes; add a script
  that records, enters replay, scrubs, and asserts the scratch grid — must
  answer DA/kitty queries like the other PTY tests, and **must isolate
  `XDG_STATE_HOME`** (and the replay dir) so it never writes the live DB or
  leaves `.szr` files in the real state dir.

## Risks & non-goals

- **Memory.** Mitigated by the dual byte/time budget; default 8 MiB/pane. The
  recorder is the only always-on addition — measured with `just bench` before/
  after, recorded as a perf delta per the perf-commit convention.
- **The playback clock** is the only new timer. Scoped to "alive only while
  playing"; audited against the zero-idle invariant above.
- **Not** importing cy's Janet API / scripting, multiplayer/multi-client
  registers, or its node-graph model — those are cy's architecture, not ours.
  We're borrowing the _capabilities_ (replay, time-search, registers,
  screen-swap), implemented natively on superzej's seams.

## Sequencing

1. **Phase 1** recorder (ring + keyframes + budget) — invisible, lands behind
   `replay.enabled = false`, measured for cost. _Nothing else works without it._
2. **Phase 2** replay overlay + playback clock + time-search — the headline.
3. **Phase 3** registers — independent, small, high polish-per-line.
4. **Phase 4** live screen-swap — only if vt100 exposes the inactive screen;
   else closed as "subsumed by replay."
