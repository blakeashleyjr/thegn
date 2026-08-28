# THE-78 Architect Review — Verdict

**APPROVED** — no revision chunks. One small correction applied by the reviewer
(commit `a775ef84`); everything else lands as specced.

Reviewed: full branch diff `git diff main...HEAD` after merging `main`
(`24181bc0`, clean — main's new commits touched only `.thegn/pipeline/THE-76`
records and `test/brand-guard.sh`, no crate code). Design:
`.thegn/pipeline/THE-78/architect/design.md`; chunks 1–2 + both done files.

## Lead-mandated gates (run this session, temp `XDG_STATE_HOME`)

- `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example) |
test(control_schema) | test(capability)'` — **26/26 pass**.
- `cargo nextest run -p thegn-host -E 'test(complete) | test(help) |
test(catalog_tests) | test(mq_assets) | test(platform_ratchet)'` — **98/98 pass**.
- Binary launched only against a throwaway repo + temp `XDG_STATE_HOME` with
  `THEGN_NO_DAEMON=1`; live state DB never touched; temp dir removed after.

## Chunk 1 (code, `4f3d1b46`) — verified against design §4

- `startup_heal.rs`: `BARRIER_TIMEOUT_MS=250`; `HealGate` (Mutex<bool>+Condvar,
  poisoned-mutex-tolerant, bounded wait loop correct under spurious wakeups);
  named `startup-heal` thread, first statement `Qos::Background`; spawn failure
  → `warn!` + uncompleted gate (fail-safe, same direction as the cpu-cap wrap);
  heal body verbatim from old run.rs:598-624 incl. the common-dir probe with its
  `#[expect]` (reason updated); `thegn::startup` waterfall event
  (`since_start_ms`/`heal_ms`/`roots`/`healed`); `complete()` **before** the
  Model send; both `let _ =` carry `// best-effort:`.
- `run.rs`: refresh channel hoisted with rationale comment; heal block replaced
  by gate + spawn (`start`@351, `waker`@537 both precede the site); startup
  hydration passes `Some(gate)`; "session loaded" event untouched; `#[expect]`
  count now exactly 2 (blame@2817 off-loop, git-init@15129 post-frame); 0 hits
  for `heal_main_checkout_worktree`/`repo::toplevel`.
- `hydrate.rs`: gate wait is the first statement inside `catch_unwind`, before
  `Db::open` — cannot panic, cannot block the loop (Utility blocking-pool task).
- `merge_sweep.rs`: `toplevel` resolved inside the existing `spawn_blocking`
  closure; `None` → no-op; `handlers/merge_queue.rs:224` semantics preserved.
- Accepted deviation (flagged in chunk-1-done): **12** `spawn_model_hydration`
  call sites vs the spec's 4 — the signature change forces every one; diff
  audited, all get `None,` except the startup site. `handlers/tracker.rs` +1
  line is the minimal mechanical consequence. Accepted.
- Ratchets: no new `#[cfg]` outside `platform/` (only `#[cfg(test)]`); no
  color/glyph literals; `idle_poll.rs`/`render_plan.rs` untouched; no new
  actions/config keys.

## Chunk 2 (docs, `f9d28177`) — verified; one correction applied

Landed verbatim per spec; `openspec validate --all --strict` 171/0 recorded by
the implementer, doc claims cross-checked here against the landed code
(`HealGate`, `BARRIER_TIMEOUT_MS`, Model+waker fixup, git-init/blame sanctioned
sites all real).

**Correction (`a775ef84`, docs-only):** the dictated sentence called both
startup git jobs "named `Background`-QoS threads". True for the heal; false for
the merge-sweep's root resolve, which runs in a **tokio `spawn_blocking` pool
thread** (default QoS). Fixed the ARCHITECTURE.md §2 sentence rather than the
code: `set_self(Qos::Background)` inside a reused blocking-pool closure would
leak the class to later tasks on that thread — the doc moves, not the code.
This also resolves chunk-2-done's flagged Unverified item.

## Unverified items from the done files — discharged by this review

Three hermetic PTY launches (fresh temp repo + worktree, temp
`XDG_STATE_HOME`, `THEGN_BENCH_FIRST_FRAME_EXIT=1`, `THEGN_LOG=info[+hydrate=debug]`):

1. **`spawn()` end-to-end** (thread → heal → gate → frame): healthy run logged
   `startup git heal since_start_ms=343 heal_ms=55 roots=3 healed=false` then
   `first frame flushed` — full chain works, no hang, zero warnings in the log.
2. **Pathological tail + the 250 ms bound** (stale main checkout via
   `update-ref` over a 380-file/60-commit repo — the fold/land shape): two
   runs, `heal_ms=1478` and `heal_ms=1749`, both `healed=true` with the main
   checkout actually fast-forwarded (0 dirty afterward). In the second run the
   hydration debug events (`sidebar status collected`, `model hydrated`) all
   logged **before** the heal completed — i.e. the gate released at the 250 ms
   bound and hydration degraded exactly as designed, while the heal's own
   Model+waker fixup fired on completion. The barrier is real and bounded.
3. **Startup waterfall re-measurement** (the THE-78 measurement ask): the heal
   remains directly measurable via the new waterfall event — that was the
   requirement; the moved sequence is byte-identical to the old one, so §2.1/2.2
   numbers stand. Absolute frame times in my runs (1.5–2.5 s, fresh state) are
   not comparable to design §2.3 (idle-box, warm daily state, 337–499 ms) — this
   box was loaded (8.5 min dep rebuild just finished); no regression signal.

Heavy gates (`just test`/coverage/e2e) deliberately not run — pre-push owns
them, per dev-loop policy and the addenda. No migration and no live-DB contact.

## Residual notes (informational, non-blocking)

- Early `Model` send / waker pulse can fire before `event_loop` takes the rx
  (~1.6 s of setup precedes it; heal is 14–22 ms warm): unbounded channel
  buffers the send; the self-pipe pulse is drained on the first poll. Benign.
- §2's amended contract is scoped to **subprocess** I/O on the launch path;
  pre-frame DB opens (`load_or_seed_session`) remain by design (§1/§3 of the
  design record the reasoning). The broader "never on the loop" claim is intact.
- e2e baselines: no frame-content change expected (first frame still built from
  the DB model; sidebar statuses arrive via unchanged post-frame hydration);
  `just e2e` at pre-push will confirm.

**Commits on branch:** `24181bc0` (merge main), `a775ef84` (review doc fix),
on top of the implementation commits `4f3d1b46`/`f9d28177`.
