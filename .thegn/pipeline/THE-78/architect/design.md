# THE-78 — Reconcile "no blocking I/O before first frame" with the startup git heal

**Architect design.** Branch `tg/the-78-first-frame-heal`. Audit finding: THE-77 F5.

## 1. Problem

`docs/ARCHITECTURE.md` §2 says: _"Never put blocking I/O on the loop — including at
startup: anything before the first frame that can block (a D-Bus probe, a network
call) runs on a thread under a cap."_

But `crates/thegn-host/clippy.toml` (host-only rules) bans synchronous child waits
in the host crate **except** at sanctioned sites carrying a local
`#[expect(clippy::disallowed_methods)]` — and it names _"startup before the loop"_
as one such legitimate site. One exists:

- **`crates/thegn-host/src/run.rs:598-624`** — the startup git heal, synchronous on
  the loop thread before the first frame:
  - `run.rs:603` `heal_main_checkout_worktree(&cwd)`
  - `run.rs:604-606` same, per session worktree-group path
  - `run.rs:613-624` a `git rev-parse --path-format=absolute --git-common-dir`
    **subprocess** (`#[expect]` at 613, reason: _"startup: runs once before the
    event loop exists"_) then a heal of the canonical checkout.
- `heal_main_checkout_worktree` (`thegn-core/src/util.rs:357-366`) on a **main
  checkout** = read `.git/config` + `git symbolic-ref` + `git rev-parse HEAD` +
  `git diff-index --quiet HEAD` (`util.rs:577-588` — 3 subprocesses on the healthy
  path); on a **linked worktree** it returns after one `is_dir`.
- The only **other** pre-first-frame subprocess on the launch path:
  `run.rs:785` `thegn_core::repo::toplevel(...)` → `git rev-parse --show-toplevel`
  (`repo.rs:12-14`), gating `merge_sweep::spawn`. Everything else pre-frame is
  already DB/fs/off-thread: `load_or_seed_session` (`hydrate.rs:1002`) and
  `session.rs` contain no git subprocess (verified by grep); devenv/direnv/nixcache
  prewarms and crash-scan are threads; `merge_guard::install` is fs-only
  (`util::git_common_dir`, `util.rs:630`, reads the `.git` file, no subprocess).

So the single source of truth states an invariant that the code contradicts in
exactly two places, and the lint config has already fossilized the contradiction
as a sanctioned carve-out. Doing nothing leaves the doc untrue (the issue's terms).

## 2. Measurement (evidence, not hypotheses)

Machine: this dev box, warm FS cache; "cold" approximated by
`posix_fadvise(POSIX_FADV_DONTNEED)` over the git binary, the thegn binary and the
repo's `.git` tree (no root, so no `drop_caches`; this evicts exactly the pages the
heal touches). Binary: `/home/blake/code/thegn/target/debug/thegn` (Aug 27) — its
heal block is identical to HEAD (last touched `1bea0f19`, 07-28; verified via
`git log -L 596,624:crates/thegn-host/src/run.rs`). Record locally; not CI.

### 2.1 Healthy path (daily launch)

The heal is **4 git subprocesses + 1 config read + N is_dir stats** (N = 1 + group
count; all 37 daily groups in the live DB are linked worktrees — `.git` is a file —
and 0 are main checkouts):

| component (exact commands from run.rs:614 / util.rs:578-586) | warm median        | cold-ish           |
| ------------------------------------------------------------ | ------------------ | ------------------ |
| `git rev-parse --git-common-dir`                             | 2.1 ms             | —                  |
| `git symbolic-ref --short HEAD` (canonical)                  | 2.3 ms             | —                  |
| `git rev-parse HEAD` (canonical)                             | 2.1 ms             | —                  |
| `git diff-index --quiet HEAD --` (canonical, 2217 files)     | 7.3 ms             | —                  |
| **whole chain**                                              | **13.9 ms** (n=25) | **21-22 ms** (n=3) |
| 38 × `.git` is_dir stats                                     | 0.18 ms            | —                  |

**Worktree count is irrelevant**: the per-group loop costs 0.18 ms at the real
daily count (37 groups) because they are linked worktrees. Cost is per _main
checkout_ probed, which is constant (canonical) in the common case.

### 2.2 Pathological tail (the reason this matters)

When a fold/land moved a branch ref elsewhere and the main checkout is stale,
`resync_stale_main_checkout` (`util.rs:597-618`) walks up to 50 ancestors × 2
`diff-index` probes. Measured with the exact command sequence on a synthetic
63-commit/400-file repo: **199-260 ms** (walk exhausted); scaled to a
2217-file repo: **~730 ms**. This tail hits exactly the fold-heavy workflow this
repo lives in (`thegn land` moves refs from worktrees; the next launch pays the
walk **on the loop, before the first frame**, today).

### 2.3 End-to-end waterfall (`THEGN_LOG=info` + `THEGN_BENCH_FIRST_FRAME_EXIT=1`, isolated `XDG_STATE_HOME`, launched from a linked worktree, fresh state)

| stage                                                                 | cold-ish   | warm (×3)  |
| --------------------------------------------------------------------- | ---------- | ---------- |
| launch → first frame                                                  | **499 ms** | 337-374 ms |
| "terminal ready" → "session loaded" (config + session + heal bracket) | 123 ms     | 46-50 ms   |
| — of which the heal itself (component-measured)                       | ~22 ms     | ~14 ms     |

The heal is _not_ the dominant launch cost (raw mode/probe ~83 ms and
pins/daemon/model ~180-210 ms are bigger), but it is the largest **pure blocking
subprocess block** sitting on the loop pre-frame, and its tail (§2.2) is the
single worst pre-frame latency source in the fold workflow. The warm bracket also
shows `load_or_seed_session` (DB + fs) + config load cost the same order as the
heal — but those are sanctioned by §9 (DB is a cache; a launch never blocks on
config — unknown keys are dropped) and are not subprocess I/O.

## 3. Decision

**Move the heal off-loop behind a bounded barrier, AND amend the docs.** Both of
the issue's options, because moving is what makes the amended doc _true_:

1. The off-loop pattern already exists **twice** for this exact operation:
   - `git_watch.rs:282-307 spawn_main_checkout_heal` runs the identical
     common-dir probe + heal off-loop on `MainRefMoved`, sending
     `RefreshKind::Model` + pulsing the waker when it healed (301-305). The
     startup site is the only place still doing it synchronously.
   - `run.rs:6684-6700 crash-scan`: startup-time named `std::thread` with
     `Qos::Background` doing DB I/O pre-frame, commented _"no work lands on the
     event loop before the first frame"_.
2. The doc's strong claim is the intent (sub-300ms launch, §Perf invariants);
   amending §2 to bless a synchronous subprocess on the launch path would weaken
   the source of truth to match a shortcut when the precedented fix is ~150 lines.
3. The barrier is cheap and self-correcting (§5): on the daily path the heal
   finishes before hydration even starts, so the barrier never waits.

Rejected alternatives:

- **Doc-only amendment** (state the carve-out §2): makes the doc true by
  weakening it; leaves a 0.2-0.8 s fold-correlated tail on the launch path that
  the two existing precedents already know how to avoid. Only defensible if the
  move were risky — it isn't (§5).
- **No barrier** (heal off-loop, first git consumer races it): self-corrects via
  the healed⇒Model pulse, but in the stray-`core.worktree` case the _first_
  hydration pass renders degraded (`git` aborts with "Invalid path" until the
  surgical config strip lands) and one refresh flash later. The barrier is ~15
  lines and removes the flash.
- **Run the heal inside the first hydration task**: couples a startup-sequencing
  concern to every refresh hydration (which re-runs constantly); the runtime heal
  for ref moves already lives in `git_watch` and must not be duplicated.

## 4. Design

### 4.1 New module `crates/thegn-host/src/startup_heal.rs` (~130 lines)

Owns the startup heal thread + the barrier. (God-file rule: new sibling module,
not run.rs. Shares `thegn_core::util::heal_main_checkout_worktree` with
`git_watch::spawn_main_checkout_heal`, which stays for `MainRefMoved`.)

```rust
//! Startup git heal, off-loop (THE-78). …docs: why (§1-2), precedents, contract.

/// Bound on how long the first git-reading consumer waits for the heal.
pub(crate) const BARRIER_TIMEOUT_MS: u64 = 250;

pub(crate) struct HealGate { /* Mutex<bool> + Condvar */ }
impl HealGate {
    pub(crate) fn new() -> std::sync::Arc<Self>;
    /// True when the heal completed (healed or not) before the deadline.
    /// Never blocks unboundedly: a lost/spawn-failed thread costs one timeout.
    pub(crate) fn wait_bounded(&self, timeout: std::time::Duration) -> bool;
    fn complete(&self);
}

/// Runs today's run.rs:603-624 sequence on a Background thread:
/// cwd + each group path + common-dir probe + canonical heal. Logs a
/// `thegn::startup` waterfall event (`heal_ms`, `healed`, `roots`) so the heal
/// stays directly measurable (THE-78's ask). Completes the gate; when it healed
/// anything, sends `RefreshKind::Model` + pulses the waker (the
/// `spawn_main_checkout_heal` fixup pattern).
pub(crate) fn spawn(
    cwd: std::path::PathBuf,
    group_paths: Vec<std::path::PathBuf>,
    start: std::time::Instant,
    waker: termwiz::terminal::TerminalWaker,
    refresh_tx: tokio_mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    gate: std::sync::Arc<HealGate>,
);
```

Thread mechanics (mirrors `crash-scan`, `run.rs:6684`):
`std::thread::Builder::new().name("startup-heal".into()).spawn(...)`; first
statement inside: `crate::platform::qos::set_self(Qos::Background)` (housekeeping
class per CLAUDE.md; no-op off macOS). The `git rev-parse --git-common-dir`
subprocess moves here with its `#[expect(clippy::disallowed_methods)]` + reason
(now sanctioned as _"closures already inside … std::thread"_ per the host
clippy.toml comment, which this change updates — §4.5). Spawn failure
(`io::Result`): `tracing::warn!` and return — the gate stays uncompleted and
waiters fall out at the timeout (bounded, self-correcting), matching the
fail-safe posture of the cpu-cap wrap.

### 4.2 `run.rs` wiring

- **Move `let (refresh_tx, refresh_rx) = tokio_mpsc::unbounded_channel::<RefreshKind>()`
  (line 885) up to the heal site (~line 598)** with a comment: the heal thread
  needs the sender before the ticker that normally creates it; channel-creation
  order is behavior-neutral (the rx still moves into `event_loop` at ~1041,
  `refresh_tx` still reaches the ticker at ~885+). Nothing between 598 and 885
  referenced it before (verified).
- **Replace the synchronous block 598-624** with: build the gate
  (`HealGate::new()`), collect `session.worktrees.iter().map(|g| PathBuf::from(&g.path)).collect()`
  (`WorktreeGroup.path` is `String`, `session.rs:224`; `session` is still owned
  here — it moves into `event_loop` at 1041), call `startup_heal::spawn(cwd.clone(), …, start, waker.clone(), refresh_tx.clone(), gate)`.
  `start` is the `Instant` from `run.rs:351` (Copy). Keep the existing
  "Defensive self-heal" comment, amended to describe the off-loop arrangement and
  the barrier.
- **`spawn_model_hydration` call at 766** (the startup one): pass
  `Some(std::sync::Arc::clone(&heal_gate))`. The other three callers (2372, 2433,
  10255 — runtime refreshes) pass `None`.
- **`run.rs:785` merge-sweep root resolve off-loop**: change
  `merge_sweep::spawn(cfg, root)` to take the launch dir and resolve the toplevel
  inside its existing blocking task (§4.4); the call site becomes
  `crate::merge_sweep::spawn(cfg.clone(), std::env::current_dir().unwrap_or_default())`.

After this, the pre-first-frame launch path has **zero** synchronous subprocess
sites (the two remaining `#[expect]`s in run.rs are the off-loop blame at 2816 and
the post-frame, explicit-user-confirm `git init` at 15126, documented on-site).

### 4.3 `hydrate.rs` — the barrier consumer

`spawn_model_hydration` (3261) gains a final param
`heal_gate: Option<std::sync::Arc<crate::startup_heal::HealGate>>`. Inside the
`spawn_blocking` task (already a Utility thread — waiting there can never touch
the loop), as the first statement of the `catch_unwind` closure (before
`Db::open`/git fan-outs):

```rust
// THE-78: the startup git heal runs concurrently on its own thread; a stray
// `core.worktree` in the shared `.git/config` makes every git call in the repo
// abort ("Invalid path"), so wait (bounded) for the heal before the first git
// read. Past the timeout (a pathological resync walk), proceed — the heal's own
// Model-refresh fixup corrects this pass when it lands.
if let Some(g) = heal_gate.as_deref() {
    g.wait_bounded(std::time::Duration::from_millis(crate::startup_heal::BARRIER_TIMEOUT_MS));
}
```

Sequencing note: the heal thread is spawned ~160 ms before hydration is spawned
(766), so on the daily path `wait_bounded` returns instantly (heal ≈ 14-22 ms).

### 4.4 `merge_sweep.rs`

`pub fn spawn(cfg: Config, dir: std::path::PathBuf)` — rename the param, resolve
`thegn_core::repo::toplevel(&dir)` as the first statement **inside** the existing
`spawn_blocking` closure; `None` → return (same effect as today's `if let` at the
call site). Callers: `run.rs:786` (launch dir) and
`handlers/merge_queue.rs:224` (passes `any_path`; `toplevel` of a path inside the
repo yields the same sweep root — semantics preserved). Doc comment updated:
the resolve is now off-thread too.

### 4.5 `crates/thegn-host/clippy.toml`

Host-only comment block: replace the sanctioned-site list item _"startup before
the loop"_ with _"closures already inside spawn_blocking / std::thread (e.g. the
startup heal thread)"_ so the lint config states where the expects now live.

### 4.6 Docs (chunk 2)

- `docs/ARCHITECTURE.md` §2: replace the _"including at startup…"_ sentence with
  the true contract: no blocking subprocess I/O on the loop **or** on the
  pre-first-frame launch path; the startup git heal and the merge-sweep root
  resolve run on Background threads; the heal completion is a bounded barrier
  (`startup_heal::HealGate`) awaited by the first git-reading consumer (initial
  model hydration); a healed checkout pulses a Model refresh. Note the two
  remaining sanctioned on-loop subprocess sites (post-frame interactive `git
init`; `src/cmd/` CLI verbs, which are not the loop) and the gate.
- `openspec/specs/event-loop/spec.md`: MODIFY the "No blocking I/O on the loop"
  requirement to cover the pre-first-frame launch path + add scenario "Startup
  heal runs off-loop behind a bounded barrier" (WHEN thegn launches THEN the heal
  runs on a Background thread, the initial hydration awaits it bounded, and a
  healed checkout wakes the loop via Model+waker).

## 5. Failure modes & invariants

| case                              | behavior                                                                                                                                           |
| --------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| heal thread spawn fails           | warn; gate never completes; hydration proceeds at 250 ms; heal retries next launch                                                                 |
| git missing / probe fails         | `healed=false`, gate completes instantly, no refresh — identical to today's no-op, just off-loop                                                   |
| stray `core.worktree`             | surgical config strip lands; hydration waited (bounded) → first pass correct; Model pulse refreshes                                                |
| pathological resync walk > 250 ms | hydration proceeds; heal's Model pulse re-hydrates when done (today: user stares at a blank sidebar for the whole walk **before the first frame**) |
| `THEGN_BENCH_FIRST_FRAME_EXIT=1`  | process may exit before the thread finishes — best-effort by design; bench measures first frame without paying the heal                            |
| two instances, same repo          | heal is idempotent (probes no-op when coherent)                                                                                                    |

Invariant checklist:

- **Render-plan**: untouched. The heal reaches the loop only as an ordinary
  `RefreshKind::Model` (existing hydration path); `render_plan::plan` tests stay
  green — no new damage class, no chrome recomposition.
- **idle_poll / one timed `poll_input`**: untouched.
- **0% idle**: healthy path sends nothing (no wake); healed path sends exactly one
  Model + one pulse.
- **QoS**: `Background` declared inside the thread (no-op off macOS).
- **Ratchets**: no new `#[cfg]` (platform-cfg); no color/glyph literals; every
  `let _ =` send/pulse carries `// best-effort:` (ignored-Result ratchet — sends
  to a possibly-gone consumer, waker pulse, same as `git_watch.rs:301-305`); no
  new actions/keys/config → help ratchet N/A; `gh` N/A.
- **Coverage**: host-only change; `thegn-core` untouched → 95% gate unaffected.
  New host logic carries unit tests (below).
- **e2e**: no frame-content change (first frame is built from the DB model at
  run.rs:732; sidebar statuses arrive via post-frame hydration, unchanged) → no
  re-record expected; optional `just e2e` spot-check at pre-PR.
- No waterfall event strings are asserted anywhere (verified: no test greps
  "session loaded"/"config loaded").

## 6. Verification performed for this design

- Component timing (python, n=25 warm / n=3 cold-ish) — §2.1.
- Drift-walk timing on a synthetic repo, exact `util.rs` command sequence — §2.2.
- Startup waterfall ×4 (1 cold-ish + 3 warm) via `script` PTY wrapper,
  `THEGN_BENCH_FIRST_FRAME_EXIT=1 THEGN_LOG=info`, isolated `XDG_STATE_HOME`,
  log parsed from `$XDG_STATE_HOME/thegn/logs/thegn.log` — §2.3.
- Pre-frame subprocess inventory by grep: `run.rs` `#[expect(clippy::disallowed_methods)]`
  sites (3: heal@613, blame@2816 off-loop, git-init@15126 post-frame);
  `hydrate.rs`/`session.rs` free of git subprocess; `repo::toplevel` = 1
  `rev-parse --show-toplevel`.
- Live DB shape (read-only): 37 worktree groups, 0 main checkouts → §2.1's
  count-irrelevance claim.

## 7. Chunks

Two chunks, **serial** (chunk 2's text describes chunk 1's landed shape).

- **Chunk 1 — code**: `startup_heal.rs` (new), `run.rs`, `hydrate.rs`,
  `merge_sweep.rs`, `crates/thegn-host/clippy.toml`.
  Commit: `fix(host): startup git heal off-loop behind a bounded gate (THE-78)`
- **Chunk 2 — docs**: `docs/ARCHITECTURE.md`, `openspec/specs/event-loop/spec.md`.
  Commit: `docs(the-78): §2 + event-loop spec state the off-loop startup heal`
