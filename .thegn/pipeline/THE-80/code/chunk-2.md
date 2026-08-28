# THE-80 — Chunk 2: Background sweep (samplers/timers) + surviving-pin rewrite

**Lane:** tg/the-80-qos-sweep · **Design:** `.thegn/pipeline/THE-80/architect/design.md` (§3a rows 3, 8; §3b; §4)
**Runs:** SECOND — **after chunk 1 has landed** (both chunks edit
`test/thread-qos-ratchet.txt`; this chunk's rewrite assumes chunk 1's six lines
are already gone). All other files are disjoint.
**No behaviour change:** `platform::qos::set_self` is a no-op off macOS
(`platform/qos.rs:92-96`); on macOS it only steers core placement.

## Files touched (exact paths)

1. `crates/thegn-host/src/forward.rs`
2. `crates/thegn-host/src/perf.rs`
3. `test/thread-qos-ratchet.txt` — delete 2 lines (`forward.rs`, `perf.rs`)
   **and** rewrite the comment layout to the final form in §3 below.

## Approach

### 1. `forward.rs` — `tgforward` (`:290`), `Background`

Fixed-cadence port sampler: sleeps `poll`→backoff between blocking container
probes; the result lands at the poll cadence regardless of core placement —
same shape as the proc sampler and metrics supervisor (both `Background`;
design §2 refinement 1). Insert as the first statement of the spawned closure,
before `// The worktree we're currently tracking` / `let mut tracked`:

```rust
            // Background: a fixed-cadence sampler (poll + backoff) — the result lands
            // at the poll cadence regardless of scheduling, like the proc/metrics samplers.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
```

### 2. `perf.rs` — `thegn-bench-window` (`:182`), `Background`

Bench-only one-shot (sleeps `THEGN_BENCH_RUN_MS`, then shutdown + one wake).
Nothing waits on it; the idle harness measures against its own sampler clock
(`test/perf/cpu-sample.sh` — the wake's lateness lands in the generous tail).
Insert as the first statement of the spawned closure, before
`std::thread::sleep(Duration::from_millis(ms));`:

```rust
            // Background: bench-only one-shot timer; the idle harness measures against
            // its own sampler clock, so wake lateness only lands in the generous tail.
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
```

### 3. `test/thread-qos-ratchet.txt` — final form

Delete the `forward.rs` and `perf.rs` entries, then consolidate **all** reasons
into the leading header block so a `just ratchet-update` regeneration
preserves them (`file_ratchet` keeps only the comment run before the first
entry — `test_support/ratchet.rs:110-116`; today's mid-file comments would be
lost). The whole file becomes (keeping the existing lines 1–17 header verbatim):

```text
# Files in crates/thegn-host/src that spawn a named long-lived thread
# (`thread::Builder::new()`) without declaring its scheduler class
# (`crate::platform::qos::set_self(...)`).
#
# A thread that says nothing runs at the default (interactive) class. On Apple
# silicon that is what decides P-core eligibility, so undeclared housekeeping —
# samplers, watchers, reapers, git fan-out — competes with the render loop for
# the cores the "everything is instant" story depends on (CLAUDE.md: "New
# long-lived threads should declare a class; the default is Interactive, which
# for background work is wrong"; see crates/thegn-host/src/platform/qos.rs).
#
# Every entry is debt: declare the class as the first statement of the thread
# body — `Utility` for work the user will notice the result of, `Background`
# for housekeeping — and delete the line. The scan is file-level, so a file
# with one declared thread does not appear here; it is a debt register, not a
# proof.
#
# Not every entry is fixable: a thread the loop synchronously waits on, or one
# driving something the user is watching, is latency-coupled and demoting it is
# the regression. Those carry their reason below and are expected to stay.
#
# SHRINK-ONLY. Enforced by `long_lived_threads_declare_a_qos_class` in
# crates/thegn-host/src/platform_ratchet_tests.rs; regenerate with
# `just ratchet-update`.
#
# THE-80 sweep: every site below is deliberately Interactive (the default
# class), with the reason written down — a declaration would be churn:
#
# - db_task.rs (`thegn-db-writer`) — synchronously awaited: `flush()` (the
#   Flush barrier) runs before a cold workspace-switch resurrect
#   (session.rs:820) and on clean exit (main.rs:944), so demoting it can stall
#   the event loop.
# - frame_writer.rs (`thegn-writer`) — the render hot path; the only honest
#   class is `Interactive`, which is already the default.
# - loading/ticker.rs (`thegn-splash-tick`) — drives an animation the user is
#   actively watching; `Background` would visibly stutter it, and `Utility`
#   would make it fight the very hydration burst the splash exists to cover.
# - pane_writer.rs (per-PTY stdin writers) — on the keystroke path (recv →
#   write_all + flush), the input counterpart of frame_writer; only
#   `Interactive` is honest, which is already the default.
#
# Also not an entry (the file declares a class on its supervisor thread, so the
# file-level scan passes it): metrics.rs's `thegn-metrics-collect` thread is
# deliberately left undeclared — its parent `recv_timeout`s on it, so a
# demotion interacts with that deadline.
db_task.rs
frame_writer.rs
loading/ticker.rs
pane_writer.rs
```

## Tests (scoped — no full-workspace builds)

```sh
just quick thegn-host                                   # clippy lib/bin
cargo nextest run -p thegn-host long_lived_threads      # the ratchet: exactly 4 entries
cargo nextest run -p thegn-host forward perf
```

## Done criteria

- [ ] `forward.rs` and `perf.rs` each declare `Background` as the first
      statement of their thread body.
- [ ] `test/thread-qos-ratchet.txt` matches §3 exactly: 4 entries, all reasons
      in the leading header block, entries byte-sorted. Verify regeneration
      round-trips it: `THEGN_RATCHET_UPDATE=1 cargo test -p thegn-host
long_lived_threads && git diff --exit-code test/thread-qos-ratchet.txt`.
- [ ] `grep -rn 'qos::set_self' crates/thegn-host/src/forward.rs crates/thegn-host/src/perf.rs` → 2 hits, both Background.
- [ ] `just quick thegn-host` clean; the scoped nextest commands above green.
- [ ] No `#[cfg]` added anywhere.
- [ ] Commit with subject **exactly**:

```
fix(the-80): declare Background QoS on the forward sampler + bench timer, rewrite the surviving pin reasons
```
