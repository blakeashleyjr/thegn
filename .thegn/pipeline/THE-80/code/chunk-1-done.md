# THE-80 — Chunk 1 complete: Utility sweep (user-visible worker threads)

**Commit:** `be6a5945` — `fix(the-80): declare QoS on the user-visible worker threads (utility sweep)`
**Branch:** `tg/the-80-qos-sweep` · **Files:** 7 (30 insertions, 9 deletions)

## What was done

Added `crate::platform::qos::set_self(crate::platform::qos::Qos::…)` as the first
statement of every spawned thread body, each with a one-line rationale comment
(house style per `push_notify.rs` / `hydrate.rs`), and deleted the
corresponding 6 pin lines from `test/thread-qos-ratchet.txt` in the same commit
(ratchet semantics: a declaration removes the file from the scan's hit set, so
pin and declaration must move atomically).

| File                    | Thread                                                               | Class        |
| ----------------------- | -------------------------------------------------------------------- | ------------ |
| `desktop_notify.rs`     | `desktop-notify-drain` (⚠ braces added to single-expression closure) | `Background` |
| `desktop_notify.rs`     | `desktop-notify`                                                     | `Utility`    |
| `handlers/plugins.rs`   | `thegn-plugin-feed`                                                  | `Utility`    |
| `handlers/plugins.rs`   | `thegn-plugin-dispatch`                                              | `Utility`    |
| `mcp_proxy/upstream.rs` | `mcp-up-{name}`                                                      | `Utility`    |
| `notify.rs`             | `notify-sound`                                                       | `Utility`    |
| `plugins.rs`            | `thegn-plugins` (⚠ braces added)                                     | `Utility`    |
| `plugins.rs`            | `thegn-plugin-respawn`                                               | `Utility`    |
| `plugins.rs`            | `thegn-plugin-once`                                                  | `Utility`    |
| `share.rs`              | `tgshare` (⚠ braces added)                                           | `Utility`    |

All 4 ⚠ single-expression closures were rewritten with braces exactly per the
chunk spec snippets; rationale comments are verbatim from the spec.

No behaviour change: `set_self` is a no-op off macOS; no `#[cfg]`, no
render-path, poll-site, or channel changes anywhere in the diff.

## Verification (scoped per dev-loop policy)

- `just quick thegn-host` — clean (clippy lib/bin, 4m43s).
- `cargo nextest run -p thegn-host long_lived_threads` — **1 passed**
  (`platform_ratchet_tests::long_lived_threads_declare_a_qos_class` green with
  the 6 pins deleted — no stale entries — and the 4 survivors intact).
- `cargo nextest run -p thegn-host desktop_notify plugins share notify` —
  **31 passed**.
- `rustfmt --edition 2024 --check` on all 6 touched Rust files — clean
  (pre-commit treefmt hook also passed on commit).
- Grep audit: exactly 10 `qos::set_self` hits across the 6 files
  (9 `Utility` + 1 `Background`), matching the spec's per-file approach table.
- Ratchet diff audit: exactly the 6 specified lines deleted, nothing else
  moved (`db_task.rs`, `forward.rs`, `frame_writer.rs`, `loading/ticker.rs`,
  `pane_writer.rs`, `perf.rs` remain).
- `#[cfg]` check: the commit diff contains zero added `#[cfg]` (platform-cfg
  ratchet unaffected).

## Spec discrepancies noted (implementation followed the authoritative per-file sections)

- Done-criteria says "12 hits, 2 Background + 10 Utility"; the actual chunk-1
  scope per its own file list and the design's §6 chunk table is **10 threads,
  9 Utility + 1 Background**. The 12 counts the design's full §3a table
  (chunk 2's `forward.rs` + `perf.rs` add the remaining 2 `Background`).
- Done-criteria says the ratchet file has "exactly 10 non-comment lines" while
  enumerating 6; the enumeration is correct — the file now has exactly 6
  non-comment lines (4 survivors + `forward.rs` + `perf.rs`).

## Unverified

- macOS behaviour (`pthread_set_qos_class_self_np` actually steering core
  placement): unverifiable on this Linux box by construction — `set_self` is
  compiled no-op here. The declarations match the existing call-site pattern
  (`push_notify.rs`, `hydrate.rs`), which is the only thing this platform can
  check.
- `just lint` / `just test` / `just ci` (full-workspace gates incl. the
  platform-cfg ratchet as a gate, e2e, coverage): deliberately not run per the
  dev-loop policy and lead addenda; `just quick thegn-host` + the scoped tests
  above are the verification. e2e unaffected (no frame change).
