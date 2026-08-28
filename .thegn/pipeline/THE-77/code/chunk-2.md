# THE-77 chunk 2 — Declare a QoS class on the background threads, and give the invariant a gate

**Finding:** F4 of `.thegn/pipeline/THE-77/architect/design.md`. Read that
section first — especially the "deliberately not in the fix set" table, which is
the part a mechanical sweep gets wrong.

## Files touched (exact)

- `crates/thegn-host/src/metrics.rs`
- `crates/thegn-host/src/bridge_sup.rs`
- `crates/thegn-host/src/sandbox_events.rs`
- `crates/thegn-host/src/platform_ratchet_tests.rs`
- `test/thread-qos-ratchet.txt` (**new**)
- `justfile` (one line in the `ratchet-update` recipe)

## Overlap / dependency

**None with chunk 1 or chunk 3 — fully file-disjoint, runs in parallel with
both.** Chunk 1 edits `test/ratchet.sh` (a different file from the `justfile`
line you add here, and a different mechanism — this chunk uses the Rust
`file_ratchet`, not the bash driver). Do not touch `test/ratchet.sh` or any
existing `test/*-ratchet.txt`.

---

## Background

CLAUDE.md: _"New long-lived threads should declare a class; the default is
Interactive, which for background work is wrong."_
`crates/thegn-host/src/platform/qos.rs:1-27` names its motivating cases:
_"background hydration, metrics polling, fs-watching and git fan-out all compete
for P-cores with the render loop."_

Fourteen sites declare a class today. Roughly thirty named, long-lived
`thread::Builder` threads do not — **including two of the three the doc names as
its own examples.** The invariant has no gate at all, which is why it drifted
exactly where it is best documented.

`qos::set_self` is a self-call and a **no-op off macOS**
(`platform/qos.rs:22-26`), so on Linux this chunk is behaviourally inert and the
risk is confined to Apple silicon scheduling.

---

## Part 1 — Declare `Background` at four sites

Each is a long-lived thread doing housekeeping nobody waits on. The call goes as
the **first statement inside the spawned closure** (QoS is a thread-self call —
see the `qos.rs` module doc), matching the existing pattern at
`crates/thegn-host/src/monitor.rs:798` and `repo_index.rs:248`.

| File:line (pre-edit)                         | Thread               | Why `Background`                                                                                                                                     |
| -------------------------------------------- | -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-host/src/metrics.rs:78`        | `thegn-metrics`      | the metrics sampler supervisor — sleeps `interval_secs` between blocking HTTP scrapes; nothing waits on it. The doc's own "metrics polling" example. |
| `crates/thegn-host/src/bridge_sup.rs:123`    | `bridge-fswatch`     | fs-watch event pump; `while rx.recv().is_ok()` for the client's lifetime. The doc's own "fs-watching" example.                                       |
| `crates/thegn-host/src/sandbox_events.rs:46` | `podman-exec-events` | blocks on `podman events` stdout for the process lifetime, writing audit rows                                                                        |
| `crates/thegn-host/src/sandbox_events.rs:54` | `podman-net-events`  | same, network events                                                                                                                                 |

For `metrics.rs:78` the closure body is `run_supervisor(config, tx, waker)` — put
the `set_self` call at the top of `run_supervisor` (`metrics.rs:83`) rather than
wrapping the closure, so the declaration sits with the loop it describes.

Add a short comment at each site saying what the thread is and why it is
background — one line, in the register of the surrounding code.

### Do NOT touch these — they are latency-coupled

Pin them in the allowlist (Part 2) with the reason written on their line:

- `crates/thegn-host/src/loading/ticker.rs:69` (`thegn-splash-tick`) — drives an
  animation the user is actively watching; `Background` on Apple silicon would
  visibly stutter it.
- `crates/thegn-host/src/db_task.rs:54` (`thegn-db-writer`) — the loop
  synchronously awaits `flush()` before a cold workspace-switch resurrect and on
  clean exit; demoting it can stall the loop.
- `crates/thegn-host/src/metrics.rs:229` (`thegn-metrics-collect`) — its parent
  `recv_timeout`s on it; a demotion interacts with that deadline. (Note this
  thread is spawned _from_ the supervisor thread you are demoting in Part 1;
  macOS may propagate QoS to pthreads spawned from a demoted thread. That is
  acceptable — the whole metrics subsystem is a poller — but do not compound it
  by declaring `Background` here explicitly.)
- `crates/thegn-host/src/frame_writer.rs:100` (`thegn-writer`) — render hot path;
  if it declared anything it would be `Interactive`, which is already the
  default, so touching it is churn.

---

## Part 2 — A shrink-only ratchet, so the rest is written debt

Follow the established pattern in `crates/thegn-host/src/platform_ratchet_tests.rs`
(read `color_literals_stay_in_the_chokepoints` at `:30` for the shape). The
helper is `thegn_core::test_support::ratchet::file_ratchet`
(`crates/thegn-core/src/test_support/ratchet.rs:139`); it is `BTreeSet`-ordered,
so it has none of the byte-vs-locale hazard chunk 1 fixes in the bash driver.

Add a test to `platform_ratchet_tests.rs`:

```rust
/// A long-lived thread that declares no QoS class runs `Interactive` — on Apple
/// silicon that makes housekeeping (samplers, watchers, reapers) eligible for
/// the performance cores it is competing with the render loop for. Every named
/// `thread::Builder` thread should declare its class on entry
/// (`platform::qos::set_self`); see the module doc in `platform/qos.rs`.
#[test]
fn long_lived_threads_declare_a_qos_class() {
    file_ratchet(
        MANIFEST,
        "thread-qos-ratchet.txt",
        &["platform/"],
        |_, body| body.contains("thread::Builder::new()") && !body.contains("qos::set_self"),
        "A named long-lived thread must declare its scheduler class with \
         `crate::platform::qos::set_self(...)` as the first statement in its \
         body (CLAUDE.md; see platform/qos.rs). Add the declaration, or — with \
         a written reason on its line — pin the file.",
    );
}
```

Notes on the predicate:

- File-level granularity, like every other ratchet here: a file with one declared
  and one undeclared thread passes. That is the accepted trade in this repo — the
  list is a debt register, not a proof.
- `file_ratchet` strips comments before calling the predicate (`code_only`), so
  prose mentioning `thread::Builder` will not trip it.
- Exclude `platform/` — `qos.rs` itself spawns threads in its own tests.
- Match on `thread::Builder::new()` (the named, long-lived form) rather than bare
  `thread::spawn`, which is dominated by short-lived stdout/stderr drain threads
  where a QoS declaration is noise.

**Generate the allowlist, do not hand-write it:**

```sh
THEGN_RATCHET_UPDATE=1 cargo test -p thegn-host long_lived_threads_declare_a_qos_class
```

Then **write the header block by hand** — `file_ratchet` preserves the leading
`#` block verbatim on regeneration (`ratchet.rs:154-159`), and that header is
where the rule and the burn-down target live. Match the tone and structure of
`test/color-literal-ratchet.txt`'s header: what the rule is, why it exists, what
an entry means, that it is SHRINK-ONLY, which test enforces it, and how to
regenerate. Add the four "latency-coupled, deliberately not fixed" files with
their reason written inline, the way `test/caret-glyph-ratchet.txt` annotates its
non-caret entries.

Confirm the generated list does **not** contain `metrics.rs`, `bridge_sup.rs` or
`sandbox_events.rs` (Part 1 fixed those), and **does** contain `loading/ticker.rs`,
`db_task.rs` and `frame_writer.rs`.

## Part 3 — Wire it into `just ratchet-update`

The recipe at `justfile:243-255` already runs
`THEGN_RATCHET_UPDATE=1 cargo test -p thegn-host ratchet` (line 244), whose
substring filter matches any test in the `platform_ratchet_tests` module — so the
new test is regenerated automatically and **no new line is needed there.**
Verify this rather than assuming: run

```sh
cargo test -p thegn-host ratchet -- --list 2>/dev/null | grep qos
```

and confirm the new test is listed. If (and only if) it is not picked up, add an
explicit line to the `ratchet-update` recipe. Either way, add a one-line comment
in the recipe's comment block (`justfile:240-242`) noting the QoS ratchet, so the
list of what gets regenerated stays readable.

## Tests to run (scoped — no full-workspace gates)

```sh
just quick thegn-host
cargo nextest run -p thegn-host platform_ratchet
cargo nextest run -p thegn-host qos          # the existing platform/qos tests
```

Do not run `just test`, `just lint`, `just ci`, `just coverage`, or e2e.

## Done criteria

- The four sites in Part 1 call `crate::platform::qos::set_self(crate::platform::qos::Qos::Background)`
  as the first statement of the thread body, each with a one-line reason.
- None of the four "do not touch" files is modified.
- `test/thread-qos-ratchet.txt` exists, is machine-generated, and carries a
  hand-written header explaining the rule, the SHRINK-ONLY contract, the
  enforcing test and the regeneration command — plus inline reasons on the four
  latency-coupled entries.
- `cargo nextest run -p thegn-host platform_ratchet` passes (all five ratchet
  tests, including the new one).
- `just quick thegn-host` clean.
- Sanity-check the ratchet actually fires: temporarily delete the `set_self` line
  from `bridge_sup.rs`, confirm `long_lived_threads_declare_a_qos_class` **fails**
  naming that file, then restore it.
- No `test/*-ratchet.txt` other than the new file is modified.

**Exact commit subject (use verbatim):**

```
fix(the-77): declare Background QoS on the sampler/watcher threads + ratchet the invariant
```
