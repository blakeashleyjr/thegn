# Chunk 1 done — Startup git heal off-loop behind a bounded gate (code)

Branch `tg/the-78-first-frame-heal`, commit `fix(host): startup git heal off-loop behind a bounded gate (THE-78)`.

## What landed

- **`crates/thegn-host/src/startup_heal.rs` (new, ~200 lines incl. tests)** — per
  design §4.1: `BARRIER_TIMEOUT_MS = 250`, `HealGate` (`Mutex<bool>` + `Condvar`,
  `new()` → `Arc`, `wait_bounded` bounded on the caller-supplied timeout, private
  `complete()`), and `spawn()` which runs the old run.rs:598-624 heal sequence on a
  named `startup-heal` Background thread (first statement
  `platform::qos::set_self(Qos::Background)`, mirrors crash-scan), emits the
  `thegn::startup` waterfall event (`since_start_ms`, `heal_ms`, `roots`, `healed`),
  completes the gate, and on any heal sends `RefreshKind::Model` + pulses the waker
  (both `// best-effort:`). Spawn failure: `tracing::warn!` + return — gate stays
  uncompleted, waiters fall out at the timeout (fail-safe). The heal body is
  factored into `run(cwd, group_paths, start) -> (healed, roots)` so it is
  unit-testable without a `TerminalWaker`; the `git rev-parse --git-common-dir`
  probe moved here with its `#[expect(clippy::disallowed_methods)]` under the
  updated reason ("off-loop: inside the startup-heal thread — see clippy.toml").
- **`run.rs`** — synchronous heal block deleted (0 hits for `heal_main_checkout_worktree`;
  `#[expect(clippy::disallowed_methods)]` count now 2: blame + git-init);
  `refresh_tx/refresh_rx` channel creation moved up to the heal site (comment
  explains why; ticker still gets its clone at its spawn site, `refresh_rx` still
  moves into `event_loop`); gate built + `startup_heal::spawn(...)` called with
  `cwd.clone()`, the group paths, `start`, `waker.clone()`, `refresh_tx.clone()`,
  the gate; startup `spawn_model_hydration` passes
  `Some(Arc::clone(&heal_gate))`; merge-sweep call now passes the launch dir
  directly (0 hits for `repo::toplevel` in run.rs); ticker comment block moved with
  the ticker spawn. "session loaded" waterfall event untouched in place.
- **`hydrate.rs`** — `spawn_model_hydration` gains final param
  `heal_gate: Option<Arc<startup_heal::HealGate>>`; as the first statement of the
  `catch_unwind` closure (before `Db::open`), a `Some` gate does
  `wait_bounded(BARRIER_TIMEOUT_MS)` with the THE-78 rationale comment. The
  guaranteed-completion-signal logic is unaffected (the wait cannot panic).
- **`merge_sweep.rs`** — `spawn(cfg, dir)` resolves
  `thegn_core::repo::toplevel(&dir)` as the first statement inside the existing
  `spawn_blocking` closure (`None` → no-op, same effect as the old `if let` at the
  startup call site); doc comment updated. `handlers/merge_queue.rs:224` caller
  unchanged, semantics preserved.
- **`clippy.toml` (host)** — sanctioned-site phrase updated: "startup before the
  loop" removed; the list now reads "(CLI subcommands in src/cmd/, closures already
  inside spawn_blocking / std::thread (e.g. the startup heal thread))". `git grep
"startup before the loop"` → no hits. `disallowed-methods` list unchanged.
- **`main.rs`** — `mod startup_heal;` declared (alphabetical position per rustfmt
  sort: after `ssh_shim`). Sanctioned by the chunk's parenthetical — sibling
  modules (`mod git_watch;` etc.) live in main.rs's module table, not run.rs.

## Deviations from the chunk spec (all mechanical or pre-approved by its text)

1. **`spawn_model_hydration` has 12 call sites, not 4.** The spec lists the startup
   site (766) plus "three callers" (~2372, ~2433, ~10255); the tree actually has
   nine run.rs sites (2372, 2433, 10255, 10914, 16280, 18165, 18193, 18231 + the
   startup one), three in hydrate.rs (`toggle_system_scope` / `toggle_across_scope`
   / `toggle_merge_scope`), and one in `handlers/tracker.rs` (`toggle_link`). The
   signature change forces every one of them: startup gets the gate, all others
   `None` (runtime refreshes). `handlers/tracker.rs` is outside the chunk's owned
   file list — the single added `None,` line is the minimal mechanical consequence
   of the spec'd signature; flagged here for review.
2. **Test 3 executed via the spec's preferred fallback.** `TerminalWaker` is not
   constructible in a hermetic test (unix `UnixTerminalWaker` has private fields
   and no public ctor), so the chunk's "prefer this factorization" path was taken:
   the thread body is `run(cwd, groups, start) -> (bool, usize)` and tests 3/4 call
   it directly (`run_on_non_repo_dir_probes_one_root_and_heals_nothing`,
   `run_counts_group_roots_even_outside_a_repo`). Tests 1/2 cover the gate
   (complete-in-thread → `wait_bounded(50ms)` true; uncompleted → `wait_bounded(5ms)`
   false, then `complete()` → true).
3. **clippy.toml phrasing merged, not duplicated.** The spec's replacement phrase
   contains the existing "closures already inside spawn_blocking / std::thread"
   item; writing both would duplicate the list. The phrase was folded into that
   item with the "(e.g. the startup heal thread)" example appended. The done
   criterion (`no hits for "startup before the loop"`) passes.
4. **Hydrate test filter**: the spec's `cargo nextest run -p thegn-host
hydrate_tests::load_or_seed` doesn't match the real module path (`hydrate::tests`,
   via `#[path = "hydrate_tests.rs"]`); ran `... thegn-host hydrate::tests::load_or_seed`.

## Verification performed

- `just quick thegn-host` — **green** (clippy -D warnings, lib+bin, includes the
  new module's fulfilled `#[expect]`).
- `cargo nextest run -p thegn-host startup_heal` — **4/4 pass**.
- `cargo nextest run -p thegn-host merge_sweep` — **2/2 pass**.
- `cargo nextest run -p thegn-host hydrate::tests::load_or_seed` — **2/2 pass**.
- `cargo fmt -p thegn-host -- --check` — clean.
- Done-criteria greps all pass (heal/`repo::toplevel` gone from run.rs; expect
  count 2; clippy.toml phrase gone).
- Invariants: no new `#[cfg]` outside `platform/` (only the standard
  `#[cfg(test)]`); both `let _ =` send/pulse carry `// best-effort:`; no
  color/glyph literals; no new actions/config keys (help ratchet untouched);
  `idle_poll.rs` / `render_plan.rs` untouched; no other files modified (`git status`
  shows only the files listed above).

## Unverified

- **`spawn()`'s runtime end-to-end behavior** (thread spawn → heal → gate
  completion → Model refresh + waker pulse) is exercised only by typecheck/clippy
  and would need a live launch or e2e — both out of scope here (e2e forbidden by
  the addenda; a launch was not performed). The gate mechanics themselves are
  unit-tested; the spawn wrapper is 10 trivial lines around the tested `run`.
- **The 250 ms bound in a real pathological scenario** (slow resync walk) — not
  reproducible hermetically; only the 5 ms timeout path is unit-tested.
- **Startup waterfall re-measurement** (`THEGN_LOG=info` + first-frame bench, the
  THE-78 measurement ask) — not run here; left to the review/verification stage.
- **Heavy gates** (`just test`, `just coverage`, e2e) — deliberately not run per
  the dev-loop policy / addenda; the pre-push hook owns them.
