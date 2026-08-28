# THE-80 — Architect design: burn down `test/thread-qos-ratchet.txt`

**Issue:** THE-80 (THE-77 / F4 / P3 follow-up) · **Lane:** tg/the-80-qos-sweep · **Role:** ARCHITECT
**Inputs:** `test/thread-qos-ratchet.txt` (12 pinned files + the metrics.rs note),
THE-77 F4 (`git show 13894334:.thegn/pipeline/THE-77/architect/design.md`),
`crates/thegn-host/src/platform/qos.rs`, CLAUDE.md "Thread QoS" bullet.

## 1. Problem

Every pinned file spawns a named long-lived `thread::Builder` thread that
declares no scheduler class, so on Apple silicon it runs at the default
`Interactive` class — eligible for the P-cores the render loop depends on. THE-77
pinned the ~30 undeclared sites as written debt and fixed its own examples; this
lane is the considered per-site sweep: for each pin, decide the class from
**latency coupling**, declare it, delete the pin — or keep the pin with a written
reason. No behaviour change on Linux (`qos::set_self` compiles to an empty
`imp::apply` off macOS, `platform/qos.rs:92-96`); the entire effect is core
placement on Apple silicon.

## 2. The rubric (from THE-77 F4's four worked examples + the taxonomy)

`platform/qos.rs:27-40` defines the classes; `qos.rs:115` is the declaration
point (first statement of the thread body). The decision rule THE-77 applied:

1. **Who waits on this thread's output, and how soon must it land?**
   - The loop _synchronously_ awaits it (`flush` barrier, deadline) → keep
     `Interactive` (default; a declaration is churn). Worked example:
     `thegn-db-writer` (`db_task::flush` blocks `main.rs:944` on clean exit and
     `session.rs:820` on workspace-switch resurrect).
   - The user is actively watching it render (splash animation) → `Interactive`.
     Worked example: `thegn-splash-tick`.
2. **Will the user notice the result, but never blocks on it?** → `Utility`
   (model hydration, "a git fan-out behind a visible panel", a pane spawn).
   In-repo precedent: model hydration (`hydrate.rs:3273` `Utility`, comment:
   _"the user WILL notice this land … but is never blocked on it"_), image paste
   (`handlers/paste_image.rs:71` `Utility`).
3. **Housekeeping on a cadence, or a pump nothing waits on?** → `Background`.
   Precedent: proc sampler (`hydrate.rs:476`), metrics supervisor
   (`metrics.rs:90`), fs-watch pump (`bridge_sup.rs:127`), push-to-phone
   publisher (`push_notify.rs:40`).

Two refinements the worked examples imply, applied below:

- A **fixed-cadence sampler feeding a visible panel is still `Background`** —
  the proc sampler feeds the Processes tab yet is `Background` because the result
  lands at the poll cadence regardless of scheduling. Cadence dominates
  visibility.
- A **pump whose consumer enforces a deadline is `Utility`, not `Background`** —
  if demotion can make the consumer _miss the deadline_ (a spurious failure, not
  just a later result), the thread is latency-coupled. Contrast: a demoted
  sampler still produces correct samples, just at its own cadence.

## 3. Per-site decisions (evidence, not hypotheses)

The ratchet's scan is file-level (`platform_ratchet_tests.rs:81`): a file with
`thread::Builder::new()` and no `qos::set_self` in code (comments are stripped,
`test_support/ratchet.rs:83-89`) is an entry. Every thread in every pinned file
was inspected; the 12 declarations below cover **every** `thread::Builder` site
in the 8 declared files, so the file-level scan passes honestly.

### 3a. Declare + unpin (12 threads, 8 files)

| #   | File:line                  | Thread                  | Class        | Evidence & rationale                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| --- | -------------------------- | ----------------------- | ------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `desktop_notify.rs:33`     | `desktop-notify`        | `Utility`    | Delivers desktop toasts (`notify-send`/osascript subprocess, `desktop_notify.rs:55-70`). The user notices the result (a toast) but is never blocked — delivery is off-loop by design (`desktop_notify.rs:5-7`). Contrast `push_notify.rs:40`, which is `Background` _for phone push_ with its own written justification ("housekeeping, not interactive") — a phone push is inherently deferred; a desktop toast competes with the session the user is actively in. Not latency-coupled enough for Interactive (a late toast is merely late), too user-facing for Background. |
| 2   | `desktop_notify.rs:27`     | `desktop-notify-drain`  | `Background` | Exists only when notifications are disabled; parks on `recv` forever, delivers nothing (`desktop_notify.rs:24-29`). Textbook housekeeping.                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| 3   | `forward.rs:290`           | `tgforward`             | `Background` | Fixed-cadence port sampler: sleeps `poll`→backoff (cap 10s+, `forward.rs:287-288`) between blocking container probes, emits `ForwardEvent`s (`forward.rs:330-352`). Same shape as the proc sampler and metrics supervisor (both `Background`): the result lands at the poll cadence regardless of core placement, and on a sandboxless tree it backs off and does nothing. Refinement 1 applies.                                                                                                                                                                              |
| 4   | `handlers/plugins.rs:730`  | `thegn-plugin-feed`     | `Utility`    | Feed bridge: forwards control-API event frames to a subscribing plugin's writer. Serves plugin output the user configured and sees; a stalled bridge is visibly stale plugin content. Nothing synchronously waits (the `host.call` acks immediately, `handlers/plugins.rs:706-715`).                                                                                                                                                                                                                                                                                          |
| 5   | `handlers/plugins.rs:817`  | `thegn-plugin-dispatch` | `Utility`    | Answers plugins' `host.call` request/response against the control API. Plugin content hangs off these round-trips — the "fan-out behind a visible panel" case in `qos.rs:35-37`.                                                                                                                                                                                                                                                                                                                                                                                              |
| 6   | `mcp_proxy/upstream.rs:89` | `mcp-up-{name}`         | `Utility`    | Upstream stdout pump feeding the JSON-RPC request loop, whose consumer enforces a per-request deadline (`recv_timeout(remaining)`, `mcp_proxy/upstream.rs:234`) whose expiry is a _transport failure feeding the breaker_ (`upstream.rs:20-24`). Demotion under load could turn contention into spurious tool-call failures — refinement 2 applies; this is the one Background-shaped pump that is latency-coupled.                                                                                                                                                           |
| 7   | `notify.rs:345`            | `notify-sound`          | `Utility`    | One-shot `sh -c` per toast (`notify.rs:343-358`), paired with the in-app toast the routing decision just emitted. The user hears the result; demoting it makes the audible cue lag the visual one. Short-lived but named, and the file-level scan pins the file either way — declaring is the honest fix.                                                                                                                                                                                                                                                                     |
| 8   | `perf.rs:182`              | `thegn-bench-window`    | `Background` | Bench-only one-shot: sleeps `THEGN_BENCH_RUN_MS`, sets `shutdown`, wakes once (`perf.rs:172-190`). Nothing waits on it, and the idle harness measures against its **own** sampler clock (`test/perf/cpu-sample.sh` reads `/proc` between t0/t1 with a generous tail — `RUN_MS = settle + window + 1500`), so wake lateness only lands in the tail. Diagnostics, not interactive.                                                                                                                                                                                              |
| 9   | `plugins.rs:100`           | `thegn-plugins`         | `Utility`    | Plugin host: fs discovery then the cadence scheduler for resident plugins (`plugins.rs:96-104`, `setup_and_schedule`). Its output _is_ visible UI content the user opted into — the hydration precedent (`Utility` for sidebar/panel content) extends to it.                                                                                                                                                                                                                                                                                                                  |
| 10  | `plugins.rs:118`           | `thegn-plugin-respawn`  | `Utility`    | One-shot delayed respawn of a crashed plugin session (`plugins.rs:112-140`); restores visibly-missing content.                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| 11  | `plugins.rs:152`           | `thegn-plugin-once`     | `Utility`    | Runs a one-shot plugin the user just invoked from the palette (`plugins.rs:145-170`); the user is actively waiting for its `OneShot` result. Closest to "a pane spawn" in the taxonomy.                                                                                                                                                                                                                                                                                                                                                                                       |
| 12  | `share.rs:211`             | `tgshare`               | `Utility`    | User-initiated port share (`Share::start`, `share.rs:176-218`); the supervisor's `Up(url)` lands in the statusbar chip the user is watching. "The user started it and will notice the result" verbatim.                                                                                                                                                                                                                                                                                                                                                                       |

### 3b. Deliberately left `Interactive` — pins stay, reasons rewritten (4 files)

| File:line              | Thread                                                                                                                                | Reason (verified this sweep)                                                                                                                                                                                                                                                                                      |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `db_task.rs:53`        | `thegn-db-writer`                                                                                                                     | The loop synchronously awaits it: `flush()` sends the `Flush` barrier (`db_task.rs:35, 89-95, 107-110`) and blocks on the ack from `main.rs:944` (clean exit, 2s budget) and `session.rs:820` (workspace-switch resurrect, 300ms budget). Demoting it can stall the loop — THE-77 worked example #2, re-verified. |
| `frame_writer.rs:99`   | `thegn-writer`                                                                                                                        | The render hot path (`writer_main` blits frames + pulses the waker). The only honest class is `Interactive`, which is already the default — a declaration is churn. THE-77 worked example #4, re-verified.                                                                                                        |
| `loading/ticker.rs:68` | `thegn-splash-tick`                                                                                                                   | Drives the splash animation the user is actively watching; `Background` would stutter it and `Utility` would make it fight the very hydration burst the splash exists to cover (startup). THE-77 worked example #1, re-verified.                                                                                  |
| `pane_writer.rs:151`   | per-PTY stdin writers (`spawn_stdin_writer`, `pane_writer.rs:146-169`; spawned per pane by `pane.rs:376` and `daemon/session.rs:328`) | On the keystroke path: `recv → write_all + flush` per keypress — the input counterpart of `frame_writer`. Echo latency is exactly what must not regress; only `Interactive` is honest, which is already the default. **This pin had no reason; this sweep writes one.**                                           |

### 3c. Re-affirmed, not re-opened: `metrics.rs`

Not an entry (the supervisor declares `Background` at `metrics.rs:90`, so the
file-level scan passes). The note records that `thegn-metrics-collect`
(`metrics.rs:231`, a short-lived per-scrape body reader) is deliberately
undeclared because its parent `recv_timeout`s on it (`metrics.rs:245`). The
sweep re-verified and **keeps** the decision: the consumer is itself `Background`
housekeeping, the thread is not long-lived, and declaring it would put a QoS
call inside a per-scrape spawn for zero user-visible gain. The note moves into
the leading header block (§4) so regeneration cannot lose it.

## 4. Ratchet end state

`test/thread-qos-ratchet.txt` ends with **exactly 4 entries**:
`db_task.rs`, `frame_writer.rs`, `loading/ticker.rs`, `pane_writer.rs` — all
deliberately-`Interactive`, all reasons written.

Layout discovery worth encoding in the file itself: on regeneration
(`file_ratchet`, `test_support/ratchet.rs:110-116`), only the **leading comment
run** survives (`take_while` from the top until the first entry line) — the
current mid-file per-entry comments (frame*writer, ticker) do **not**. The
end-state therefore puts \_all* surviving reasons in the leading header block as
one bullet list, followed by the four byte-sorted entries. A future
`just ratchet-update` then round-trips the file verbatim — the manual
"restore the per-entry reasons from the diff" trap is closed for this ratchet.

## 5. Why this is no behaviour change (invariants honored)

- **Linux/CI:** `set_self` is an empty call off macOS (`qos.rs:92-96`) — no
  syscall, no wake source, no channel, no polling. The 0%-idle and render-plan
  invariants are untouched (no new poll sites, no render-path code).
- **Platform seam:** no `#[cfg]` is added anywhere — `qos::set_self` _is_ the
  platform seam's public API, so `platform-cfg-host-ratchet` is unaffected.
- **Ratchet semantics:** declaring a class removes a file from the scan's hit
  set, so each declaration commit must delete its pin lines **in the same
  commit** or the stale-entry check (`ratchet.rs:133-139`) fails. Both chunks do
  so atomically; every commit is green.
- **Idempotent, best-effort:** `set_self` is documented best-effort (`qos.rs:110-118`);
  calling it inside a thread body cannot fail the thread or change its logic.
- **Coverage gate:** untouched (no `thegn-core` change).

## 6. Chunks

Two chunks. **They share `test/thread-qos-ratchet.txt`, so the Lead must run
them serially — chunk 1 first, chunk 2 second.** All other files are disjoint.
Chunk 1's ratchet edit deletes exactly its six pin lines (the test stays green:
the shared reason comment above `db_task.rs` still covers the remaining
entries); chunk 2 deletes its two lines and performs the final reason rewrite
(§4). Full specs: `.thegn/pipeline/THE-80/code/chunk-{1,2}.md`.

| Chunk | Scope                                | Files                                                                                                                                                          | Threads |
| ----- | ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------- |
| 1     | Utility sweep — user-visible workers | `desktop_notify.rs` (2), `handlers/plugins.rs` (2), `mcp_proxy/upstream.rs` (1), `notify.rs` (1), `plugins.rs` (3), `share.rs` (1) + ratchet (6 lines deleted) | 10      |
| 2     | Background sweep + pin hygiene       | `forward.rs` (1), `perf.rs` (1) + ratchet (2 lines deleted; header rewritten per §4)                                                                           | 2       |

## 7. Explicitly not changed

- `metrics.rs` (§3c — re-affirmed).
- Already-declared sites (`hydrate.rs`, `push_notify.rs`, `bridge_sup.rs`,
  `sandbox_events.rs`, `monitor*.rs`, `repo_index.rs`, `model_proxy_daemon.rs`,
  `handlers/{startup,paste_image}.rs`, `pipeline_board/action.rs`, `run.rs`) —
  conforming; the push-notify `Background` contrast is noted in §3a as context,
  not a defect to "fix".
- `std::thread::spawn` sites (e.g. the proc sampler) — the ratchet's rule is
  named `thread::Builder` threads only; widening it is a different change.
- No doc/help updates needed: no new actions, keybinds, panel sections, or
  config keys (help ratchets unaffected). No user-facing frame changes (e2e
  unaffected).
