# Chunk 1 — Startup git heal off-loop behind a bounded gate (code)

**Depends on:** nothing. **Overlap:** none — this chunk owns the five files below;
chunk 2 (docs) is file-disjoint but must run AFTER this chunk (its text describes
this chunk's landed shape). Design: `.thegn/pipeline/THE-78/architect/design.md`
(§4 = the normative spec for every edit below).

## Files touched (exact paths)

1. `crates/thegn-host/src/startup_heal.rs` — **new**
2. `crates/thegn-host/src/run.rs`
3. `crates/thegn-host/src/hydrate.rs`
4. `crates/thegn-host/src/merge_sweep.rs`
5. `crates/thegn-host/clippy.toml`

No other file may change. (`crates/thegn-host/src/main.rs` needs no edit if the
module is declared in run.rs's module table — check where sibling modules like
`git_watch` are declared (`mod git_watch;` in main.rs, line ~24 style) and declare
`mod startup_heal;` in the same place, alphabetically.)

## Approach

### 1. `startup_heal.rs` (new)

Per design §4.1, verbatim contract:

- `pub(crate) const BARRIER_TIMEOUT_MS: u64 = 250;`
- `pub(crate) struct HealGate` — `Mutex<bool>` + `Condvar`, constructed via
  `pub(crate) fn new() -> std::sync::Arc<Self>`.
  - `pub(crate) fn wait_bounded(&self, timeout: Duration) -> bool` —
    `Condvar::wait_timeout` until the bool flips or the deadline; returns
    whether it completed. Must never block unboundedly.
  - `fn complete(&self)` — set the flag + `notify_all`.
- `pub(crate) fn spawn(cwd: PathBuf, group_paths: Vec<PathBuf>, start: Instant,
waker: TerminalWaker, refresh_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
gate: Arc<HealGate>)`:
  - `std::thread::Builder::new().name("startup-heal".into()).spawn(move || { … })`;
    on `Err` `tracing::warn!` and return (gate stays uncompleted → bounded waits
    time out — fail-safe).
  - First statement in the closure:
    `crate::platform::qos::set_self(crate::platform::qos::Qos::Background);`
    (mirrors crash-scan, `run.rs:6687`).
  - Body = today's `run.rs:598-624` sequence, moved verbatim:
    1. `thegn_core::util::heal_main_checkout_worktree(&cwd)`, tracking whether it
       returned true;
    2. same per `group_paths`;
    3. the `git rev-parse --path-format=absolute --git-common-dir` probe
       (`thegn_core::util::git_cmd(&cwd)…output()`) **keeping its
       `#[expect(clippy::disallowed_methods)]`** with the updated reason
       ("off-loop: inside the startup-heal thread — see clippy.toml"), then
       `heal_main_checkout_worktree(&common_parent)` on success, tracking the bool;
    4. waterfall event (keeps the heal measurable — THE-78's measurement ask):
       ```rust
       tracing::info!(target: "thegn::startup",
           since_start_ms = start.elapsed().as_millis() as u64,
           heal_ms = t0.elapsed().as_millis() as u64,
           roots = <count probed>, healed = <any true>,
           "startup git heal");
       ```
    5. `gate.complete();`
    6. if anything healed:
       `let _ = refresh_tx.send(crate::hydrate::RefreshKind::Model); // best-effort: consumer may be gone`
       and `let _ = waker.wake(); // best-effort: loop may be gone`
       (the `spawn_main_checkout_heal` fixup pattern, `git_watch.rs:301-305`).
  - Preserve the "Defensive self-heal" comment from `run.rs:598-602` (adapted:
    it now _starts_ before the first frame and completes off-loop; hydration
    awaits it bounded).

### 2. `run.rs`

- **Move** `let (refresh_tx, refresh_rx) = tokio_mpsc::unbounded_channel::<RefreshKind>();`
  (currently line 885) up to the old heal site (~line 598), with a comment: the
  startup heal thread needs the sender before the refresh ticker (which still
  receives its clone at its spawn site ~line 903); channel-creation order is
  behavior-neutral (`refresh_rx` still moves into `event_loop` at ~1041).
  Leave the ticker's own comment block attached to the ticker spawn.
- **Delete** the synchronous heal block (598-624 through the `#[expect]` at 613)
  and replace with:
  ```rust
  let heal_gate = crate::startup_heal::HealGate::new();
  crate::startup_heal::spawn(
      cwd.clone(),
      session.worktrees.iter().map(|g| std::path::PathBuf::from(&g.path)).collect(),
      start,
      waker.clone(),
      refresh_tx.clone(),
      std::sync::Arc::clone(&heal_gate),
  );
  ```
  (`start` = the `Instant` from line 351; `session` still owned here — it moves
  into `event_loop` at ~1041; `WorktreeGroup.path` is `String`, `session.rs:224`.)
  The "session loaded" waterfall event at ~625 stays where it is (it now logs
  before the heal finishes; nothing asserts its timing — verified).
- **Line ~766** `spawn_model_hydration(...)`: add final arg
  `Some(std::sync::Arc::clone(&heal_gate))`. The other three callers (lines
  ~2372, ~2433, ~10255) get `None` (runtime refreshes — gate already resolved).
- **Line ~785-787**: replace
  ```rust
  if let Some(root) = thegn_core::repo::toplevel(&std::env::current_dir().unwrap_or_default()) {
      crate::merge_sweep::spawn(cfg.clone(), root);
  }
  ```
  with `crate::merge_sweep::spawn(cfg.clone(), std::env::current_dir().unwrap_or_default());`
  (toplevel resolve moves into the sweep thread — §4.4).

### 3. `hydrate.rs`

- `spawn_model_hydration` (line 3261) gains final param
  `heal_gate: Option<std::sync::Arc<crate::startup_heal::HealGate>>`.
- Inside the `task::spawn_blocking` closure, as the FIRST statement of the
  `catch_unwind` closure (before `Db::open` — the wait cannot panic, so the
  guaranteed-completion-signal logic at 3274-3283 is unaffected):
  ```rust
  // THE-78: the startup git heal runs concurrently on its own thread; a stray
  // `core.worktree` in the shared `.git/config` makes every git call in the
  // repo abort ("Invalid path"), so wait (bounded) for the heal before the
  // first git read. Past the timeout (a pathological resync walk) proceed —
  // the heal's own Model-refresh fixup corrects this pass when it lands.
  if let Some(g) = heal_gate.as_deref() {
      g.wait_bounded(std::time::Duration::from_millis(
          crate::startup_heal::BARRIER_TIMEOUT_MS,
      ));
  }
  ```

### 4. `merge_sweep.rs`

- `pub fn spawn(cfg: Config, dir: std::path::PathBuf)` — rename param; inside the
  existing `spawn_blocking` closure, first statements:
  ```rust
  let Some(repo_root) = thegn_core::repo::toplevel(&dir) else { return };
  ```
  then `sweep(&cfg, &repo_root, false)` as before. Update the doc comment (the
  resolve is now off-thread too; `None` → no-op, same as the old `if let` at the
  startup call site). Caller `handlers/merge_queue.rs:224` passes `any_path` —
  unchanged text, semantics preserved (`toplevel` of a path inside the repo is
  the same sweep root).

### 5. `clippy.toml` (host)

In the host-only comment block, replace the sanctioned-site phrase
_"startup before the loop"_ with
_"closures already inside spawn_blocking / std::thread (e.g. the startup heal thread)"_
so the lint config states where the expects now live. The `disallowed-methods`
list itself is unchanged.

## Tests

Write (in `startup_heal.rs` `#[cfg(test)] mod tests`, hermetic — no live repo):

1. `gate_completes_and_wait_returns_true` — `HealGate::new()`, `complete()` in a
   thread, `wait_bounded(Duration::from_millis(50))` → true, fast.
2. `wait_times_out_on_uncompleted_gate` — `wait_bounded(Duration::from_millis(5))`
   → false (uses the param, NOT the 250 ms const — no slow tests).
3. `spawn_completes_gate_on_non_repo_dir` — temp dir without `.git`:
   `spawn(tmp, vec![], Instant::now(), <waker>, <fresh RefreshKind channel .0>, gate)`
   → `wait_bounded(2s)` true, no panic. (Probe fails → healed=false → no send —
   assert `refresh_tx` channel is still open/empty if cheap.) Use a real
   `TerminalWaker` if constructible in tests, else factor the thread body into a
   `run(cwd, groups, …) -> (bool, usize)` inner fn the spawn wraps, and unit-test
   that directly — prefer this factorization, it keeps the thread wrapper trivial.

Run (scoped — per dev-loop policy; NOT `just test`/`just ci`):

```sh
just quick thegn-host
cargo nextest run -p thegn-host startup_heal
cargo nextest run -p thegn-host merge_sweep
cargo nextest run -p thegn-host hydrate_tests::load_or_seed
```

## Done criteria

- [ ] `grep -n "heal_main_checkout_worktree" crates/thegn-host/src/run.rs` → no hits.
- [ ] `grep -c "expect(clippy::disallowed_methods)" crates/thegn-host/src/run.rs` → **2**
      (blame ~2816, git-init ~15126; the startup one moved into `startup_heal.rs`).
- [ ] `grep -n "repo::toplevel" crates/thegn-host/src/run.rs` → no hits (moved into `merge_sweep.rs`).
- [ ] `git grep -n "startup before the loop" crates/thegn-host/clippy.toml` → no hits.
- [ ] `just quick thegn-host` green (typecheck + clippy -D warnings, incl. the
      new module's expects/comments).
- [ ] Scoped nextest commands above green.
- [ ] No new `#[cfg]` outside `platform/`; every `let _ =` send/pulse carries a
      `// best-effort:` comment; no color/glyph literals; no new actions/config
      keys (help ratchet untouched).
- [ ] Invariants untouched: `idle_poll.rs`, `render_plan.rs` not modified.
- [ ] **Exact commit subject (single commit):**
      `fix(host): startup git heal off-loop behind a bounded gate (THE-78)`

Heavy gates (`just test`, e2e) are the pre-push hook's job — do not run them here.
