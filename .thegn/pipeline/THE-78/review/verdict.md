# THE-78 Security/Test/Bug Review — Verdict

PASS

**PASS** — ready for the merge queue (`thegn integrate`). One concrete finding was
caught and **fixed on this branch by this review** (commit `ddecb929`); two
residual notes are recorded below as non-blocking with rationale.

Reviewed: full branch diff `git diff main...HEAD` after confirming main is an
ancestor of HEAD (merge-base = main tip `9715b74a`; `git merge main` → "Already
up to date", nothing to resolve). Lane docs read in full: architect/design.md,
chunk-1/2 + both done files, architect-review/verdict.md — every "Unverified"
section was used as a checklist and is discharged under "Live verification"
below.

## Finding 1 (FIXED): ignored-Result ratchet violation — `just ci` would fail

`crates/thegn-host/src/startup_heal.rs` is a new file containing two
`let _ =` lines (the Model send + waker pulse). The ignored-result ratchet
(`test/ratchet.sh ignored-result 'let _ = |…|\.ok\(\);' crates`, run by
`just lint` inside `just ci`) is file-level "matches ⇒ must be pinned", and no
ratchet file was touched by the branch. Verified failing before the fix:

```
ERROR: ratchet(ignored-result): new violation in crates/thegn-host/src/startup_heal.rs
```

It slipped through because the ratchet is a bash/justfile gate, not part of the
nextest suite the pre-push hook or the mandated test list runs. **Fix
(`ddecb929`):** pinned the file in `test/ignored-result-ratchet.txt` with a
reason (the sends are the sanctioned best-effort pattern — channel send to a
possibly-gone consumer + waker pulse, `// best-effort:` commented in-file,
mirroring `git_watch.rs` — and are unfixable without changing semantics).
Ratchet re-run clean. The `let _ =` ignores themselves were reviewed and are
deliberate: the heal's fixup must never take down the thread, and failure is
benign (next refresh repaints).

## Finding 2 (residual, accepted): merge-sweep startup race vs the heal

The startup `merge_sweep::spawn` runs concurrently with the heal and is **not**
gated. Pre-change, the sweep always observed a healed tree (the heal was
synchronous and ran first). Now, if the sweep's `repo::toplevel` loses the race
against the heal thread in a repo with a stray `core.worktree`, git aborts →
`None` → the sweep silently no-ops and merged-worktree collection defers to the
next launch (the sweep has no timer). Accepted as non-blocking because it needs
three rare conditions stacked (poisoned config at launch — the exact pathology
the heal repairs, which the heal thread strips as its _first_ fs action before
any subprocess; sweep winning that race despite spawning ~100 lines of setup
later; `[merge_queue]` entries past grace), and the harm is a one-launch GC
deferral that self-corrects. If the lead wants the pre-change guarantee
restored exactly: thread `Option<Arc<HealGate>>` into `merge_sweep::spawn` and
`wait_bounded(BARRIER_TIMEOUT_MS)` as the first statement of its blocking
closure (~6 lines, runtime caller passes `None`). Deliberately not applied by
this review — it reopens an approved signature post-architect-review, which is
a design call, not a review fix.

## Finding 3 (residual, accepted): untestable/unobservable edges

- **`spawn()` failure path has no unit test** — `Builder::spawn` failure can't
  be forced hermetically. The path is 5 lines, fail-safe by construction
  (`warn!` surfaced to the `thegn::startup` target; gate left uncompleted →
  waiters fall out at the timeout), same direction as the cpu-cap wrap.
- **Poisoned-mutex tolerance untested** — can't poison hermetically without a
  panicking locker; the `unwrap_or_else(|e| e.into_inner())` pattern is the
  standard form used elsewhere.
- **Common-dir probe failure is silent** (`ok().filter(success)…`) — verbatim
  move of the pre-change code, and observable on the waterfall line (`roots`
  drops from 3 to 2), which is enough for a launch-time diagnostic.

## Adversarial code review — clean

- **Gate correctness** (`HealGate`): flag+`Condvar`, `wait_bounded` re-checks
  under the lock (spurious-wakeup safe), deadline via `checked_duration_since`
  (no overflow, `wait_timeout(0)` degrades to a poll), poisoned mutex
  tolerated. `complete()` is private, called **before** the Model send, and
  late completion after a timed-out wait is still observable (unit-tested).
- **Swallowed errors:** all `Result` ignores in the new code are the two
  sanctioned best-effort sends (Finding 1); spawn failure is `warn!`ed, not
  swallowed; probe failure is observable via `roots` (Finding 3).
- **Injection/paths/permissions:** no new attack surface — `git_cmd` arg-array
  (no shell) with scrubbed env, unchanged from the moved code; heal inputs are
  the launch dir + DB session paths exactly as before; all writes remain the
  surgical `.git/config` edit inside probed main checkouts (thegn-core,
  untouched by this branch, was reviewed in THE-77).
- **Boundedness of the gate:** first frame does **not** wait on the gate at all
  (it is built from the DB model; the gate is awaited inside the hydration
  `spawn_blocking` task), and shutdown does not join the detached heal thread —
  a hung `git` costs the hydration pass ≤250 ms and nothing else. Verified by
  construction and by the architect's logged pathological runs
  (`heal_ms=1478`/`1749` → gate released at the bound, hydration degraded, the
  heal's Model+waker fixup fired on completion).
- **QoS / waker discipline:** `set_self(Qos::Background)` is the first
  statement of the named `startup-heal` thread (thread-qos ratchet passes —
  file contains `qos::set_self`); every off-thread signal is send **and** pulse;
  healthy path sends nothing (no wake). `idle_poll.rs`/`render_plan.rs`
  untouched.
- **Channel hoist** (`refresh_tx/rx` created at the heal site): behavior-neutral
  — unbounded channel buffers an early Model send; `refresh_rx` still moves
  into `event_loop`; the ticker still clones `refresh_tx` at its own spawn site
  (verified in the diff).
- **Signature ripple:** all 12 `spawn_model_hydration` sites audited; exactly
  one (startup, `run.rs:776`) gets `Some(gate)`, the other 11 get `None` —
  correct, since runtime refreshes are gated by construction (the gate has
  resolved long before).
- **Sanctioned-site accounting:** exactly 2 `#[expect]`s remain in run.rs
  (off-loop blame, post-frame user-confirmed `git init`); the moved probe's
  `#[expect]` in startup_heal.rs is fulfilled (`just quick thegn-host` green
  under `-D warnings`, where an unfulfilled expect is an error). clippy.toml no
  longer sanctions "startup before the loop" (0 grep hits).
- **Docs match code:** ARCHITECTURE.md §2 and the event-loop spec scenario
  name real symbols (`startup_heal::spawn`, `HealGate`, `BARRIER_TIMEOUT_MS`,
  Model+waker fixup) and correctly describe the sweep's resolve as a
  `spawn_blocking` task, not a Background thread (architect's `a775ef84`
  correction verified against the code). Spec substance ("no synchronous
  subprocess before the first frame") matches the audited launch path: the two
  remaining pre-frame consumers of git are now both off-loop
  (`startup_heal::spawn`, `merge_sweep::spawn`); `git_common_dir` (merge guard)
  and `mq_assets` seeding are fs/DB-only, re-verified.

## Lead-mandated gates (run this session)

- `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example)
| test(control_schema) | test(capability)'` — **26/26 pass**.
- `cargo nextest run -p thegn-host -E 'test(complete) | test(help) |
test(catalog_tests) | test(mq_assets) | test(platform_ratchet) |
test(render_plan)'` — **118/118 pass** (includes render-plan invariants and
  the new gate tests).
- `cargo nextest run -p thegn-host -E 'test(startup_heal) | test(merge_sweep)'`
  — **6/6 pass**.
- `just quick thegn-host` — **green** (clippy `-D warnings`, lib+bin).
- `test/ratchet.sh ignored-result …` — **clean** after `ddecb929`.

## Live verification (hermetic PTY launches; scratch repos, temp `XDG_STATE_HOME`, `THEGN_NO_DAEMON=1`, `THEGN_BENCH_FIRST_FRAME_EXIT=1`, `THEGN_LOG=info[+hydrate=debug]`; live DB untouched)

- **First-frame measurement (the addenda ask), scratch `XDG_STATE_HOME`, fresh
  state, debug binary, this (loaded) box — launch → "first frame flushed":
  2276 / 2011 / 3396 ms across 3 runs.** The heal line logged
  `heal_ms=73-91`, completing ~1.4-2.7 s _before_ the frame — i.e. the heal is
  fully off the pre-frame path and the bounded barrier was resolved long before
  it mattered. Absolute totals are not comparable to design §2.3 (337-499 ms,
  warm daily state, idle box, machine-dependent and excluded from CI by policy);
  the THE-78 claim is the ordering, and the ordering is verified: zero
  synchronous subprocess sites remain pre-frame.
- **Stale main checkout (the fold/land shape), reproduced independently:**
  pre-launch `diff-index` exit=1 → launch → `startup git heal … heal_ms=176
roots=3 healed=true`, first frame flushed normally → post-launch
  `diff-index` exit=0 (checkout actually fast-forwarded). The off-loop heal +
  gate + waterfall event work end-to-end, discharging chunk-1-done's top
  "Unverified" item.
- **Healthy path:** no warnings, no hang, hydration completes post-frame;
  `model hydrated` logged after the gate released.

## e2e / frame statement (no e2e run, per addenda)

No snapshot re-recording expected. The first frame's content source (DB model)
is untouched; sidebar statuses already arrived via post-frame hydration before
this change, and the heal's end state is identical (synchronous pre-frame
before; gate-gated + Model-fixup now — final state converges either way). The
change alters _timing_ (frame 1 gets earlier by the heal's 14-22 ms warm), not
frame content, and adds no volatile chrome. The only scenario that could flap a
snapshot is a fixture repo left with a stale main checkout _and_ a >250 ms heal
(hydration would briefly render pre-heal data before the fixup pass) — no
committed fixture does this; `just e2e` at pre-push will confirm.

## Commits added by this review

- `ddecb929` `fix(the-78): pin startup_heal.rs ignored-Result ratchet entry (review)`
- verdict commit (this file) — `docs(the-78): security/test/bug review verdict`
