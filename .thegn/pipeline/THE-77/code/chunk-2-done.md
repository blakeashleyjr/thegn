# THE-77 chunk 2 — done

**Commit:** `f195b8da` — `fix(the-77): declare Background QoS on the sampler/watcher threads + ratchet the invariant`

Finding F4: long-lived background threads did not declare a QoS class, and the
invariant had no gate. Six files touched, exactly the set the spec names.

## Part 1 — `Background` declared at four sites

| File                                      | Thread               | Where the call went                                                                                                |
| ----------------------------------------- | -------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `crates/thegn-host/src/metrics.rs`        | `thegn-metrics`      | first statement of `run_supervisor` (per the spec's exception, so the declaration sits with the loop it describes) |
| `crates/thegn-host/src/bridge_sup.rs`     | `bridge-fswatch`     | first statement in the spawned closure                                                                             |
| `crates/thegn-host/src/sandbox_events.rs` | `podman-exec-events` | first statement in the spawned closure (the closure was `move \|\| subscribe_exec(tx)`; it is now a block)         |
| `crates/thegn-host/src/sandbox_events.rs` | `podman-net-events`  | same, for `subscribe_network`                                                                                      |

Each carries a one-line comment saying what the thread is and why it is
background. All four use the full path form
`crate::platform::qos::set_self(crate::platform::qos::Qos::Background)`,
matching `monitor.rs:798` / `handlers/startup.rs:116`.

The four latency-coupled files (`loading/ticker.rs`, `db_task.rs`,
`metrics.rs`'s `thegn-metrics-collect`, `frame_writer.rs`) were **not**
modified — confirmed by the commit's file list above.

## Part 2 — `test/thread-qos-ratchet.txt` + the enforcing test

`long_lived_threads_declare_a_qos_class` added to
`crates/thegn-host/src/platform_ratchet_tests.rs`, predicate and message
verbatim from the spec, `platform/` excluded.

The allowlist was **generated**
(`THEGN_RATCHET_UPDATE=1 cargo test -p thegn-host long_lived_threads_declare_a_qos_class`),
not hand-written; the header block was then written by hand. Twelve entries:

```
db_task.rs  desktop_notify.rs  forward.rs  frame_writer.rs
handlers/plugins.rs  loading/ticker.rs  mcp_proxy/upstream.rs  notify.rs
pane_writer.rs  perf.rs  plugins.rs  share.rs
```

Checked as the spec asks: `metrics.rs`, `bridge_sup.rs` and `sandbox_events.rs`
are **absent**; `loading/ticker.rs`, `db_task.rs` and `frame_writer.rs` are
**present**. Those three carry their "latency-coupled, deliberately not fixed"
reason inline above the entry.

Two deliberate deviations from a literal reading of the spec, both to keep a
decision from being lost:

1. `metrics.rs` is not an entry (file-level granularity: the file now declares a
   class on its supervisor thread, so the scan passes it), so the
   `thegn-metrics-collect` reason could not be written on an entry line. It is
   recorded in a trailing comment block in the same file instead.
2. `file_ratchet` preserves only the **leading** `#` block on regeneration
   (`ratchet.rs:154-159`), so the per-entry reasons and that trailing block are
   dropped by `just ratchet-update`. The header now says so explicitly ("restore
   the per-entry reasons below from the diff if you regenerate"). This is a
   pre-existing property of the tooling, shared with
   `test/caret-glyph-ratchet.txt`; not fixed here — out of chunk scope.

No other `test/*-ratchet.txt` was touched (see the commit file list).

## Part 3 — `just ratchet-update`

Verified rather than assumed:

```
$ cargo test -p thegn-host ratchet -- --list | grep -i qos
platform_ratchet_tests::long_lived_threads_declare_a_qos_class: test
```

The existing `cargo test -p thegn-host ratchet` line already covers it, so **no
new recipe line was added**. The recipe's comment block gained one line noting
that the first line covers the whole `platform_ratchet_tests` module,
`thread-qos-ratchet.txt` included.

## Verification run

- `cargo nextest run -p thegn-host platform_ratchet` — **5 passed** (all four
  pre-existing ratchets plus the new one).
- `cargo nextest run -p thegn-host qos` — **2 passed**
  (`platform::qos::tests::setting_a_class_is_safe_and_reversible_on_every_platform`
  plus the new ratchet). The macOS-only
  `macos_actually_applies_the_requested_class` is cfg'd out on this host.
- `just quick thegn-host` — clean, no warnings.
- **Negative test, as required:** deleted the `set_self` line from
  `bridge_sup.rs`; `long_lived_threads_declare_a_qos_class` failed with
  `new violation in ["bridge_sup.rs"]`. Restored with
  `git checkout HEAD -- crates/thegn-host/src/bridge_sup.rs`; working tree
  clean, the line is back.
- `just test` / `just lint` / `just ci` / `just coverage` / e2e — not run, per
  the dev-loop policy.

## Unverified

- **The QoS calls are behaviourally inert on this host.** `qos::set_self` is a
  no-op off macOS (`platform/qos.rs:93-107`), so nothing here was observed to
  change scheduling. The macOS FFI is covered by the existing (skipped-here)
  `macos_actually_applies_the_requested_class`. The claim that
  `Qos::Background` is the _right_ class for these four threads rests on the
  spec's reasoning, not on a measurement.
- **QoS propagation to child pthreads was not tested.** The spec flags that
  macOS may propagate the demoted class from `thegn-metrics` to the
  `thegn-metrics-collect` thread it spawns, and accepts that. Unverified either
  way from Linux.
- `sandbox_events.rs`'s two threads only spawn when `podman` is on PATH
  (`spawn` early-returns otherwise), so the edited closures were compiled but
  not executed in any test run here.
- No full-workspace build was run, so nothing outside `thegn-host` (and the
  crates it pulls in for `just quick`) was type-checked against these changes.
  The change adds no public API, so the blast radius should be nil.
