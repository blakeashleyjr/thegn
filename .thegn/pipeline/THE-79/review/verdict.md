# THE-79 — Security / test / bug review verdict

**Branch:** `tg/the-79-podman-seam` · **Reviewer lane:** security/test/bug · **Date:** 2026-08-28

PASS

## Lead addenda compliance

- **`git merge main` done first** (`cd7946d1`). main had moved 27 commits since the branch's
  last merge (THE-86 pipeline v3 fold, THE-80 QoS sweep review fixes); the merge was
  conflict-free. All checks below ran on the merged tree.
- **Lane docs read in full**: `architect/design.md`, `code/chunk-{1,2,3}.md` +
  `code/chunk-{1,2,3}-done.md` (every "Unverified" section), `architect-review/verdict.md`
  (including its accepted deviations and unlisted micro-deltas).
- **Full branch diff reviewed** (`git diff main...HEAD`: 2 core modules, sandbox.rs table
  column, host orchestrator rewrite, run.rs call-site, svc registry enrichment, justfile ×2
  keys, ratchet seed).

## Fixes applied by this review (committed)

1. **`74388b84` — die events stopped pruning (real behavioral regression in the seam move).**
   The pre-seam host code pruned `container_events` on _every_ exec-stream event — the exec
   stream carries both `exec` and `die` statuses (argv filters `event=exec`, `event=die`).
   The moved `persist` narrowed the prune to `ev.kind == "exec"` (status, not stream), so
   `die`-heavy stretches no longer bounded the table. The chunk spec's asymmetry was
   exec-stream vs network-stream, not exec-status; the existing test only pinned exec vs
   network. Fixed to `ev.kind != "network"` (the network stream is the sole producer of that
   kind), with a new test `die_events_prune_like_exec_events` pinning it.
2. **`f04c6e57` — clippy `field_reassign_with_default` in the host module's test helper.**
   Surfaced by the mandated `cargo clippy -p thegn-host --tests`; prior lanes ran
   `just quick thegn-host` (lib/bin only, per dev-loop policy) so the test-target lint was
   never seen. `just lint` runs `--all-targets -D warnings` — this would have failed the
   merge queue. Struct-init with `..Default::default()`.
3. **`e1d8e43c` — `just lint` blocked by a main-inherited violation, not THE-79.**
   `crates/thegn-host/src/daemon/pipeline_retry.rs:109` (`let _ = code;`, from THE-86's
   `59232222`) trips the ignored-result ratchet and is absent from its shrink-only list —
   **main itself is red on `just lint`**; inherited by the merge, it would have bounced
   THE-79's integrate. Fixed with an underscore parameter (behavior-neutral, doc comment
   updated, no new entry forced onto the shrink-only list). Flagged for the THE-86 lane.

## Adversarial checks — findings and dispositions

- **Swallowed errors**: audited every error path in the new modules. All `.ok()` / `let _ =`
  sites are the sanctioned best-effort shapes with comments (thread-spawn, channel send to a
  possibly-gone consumer, child reap — reap upgraded from silent `let _` to a debug log).
  `persist`'s insert/prune failure → 0 rows ⇒ no panel pulse, row stays written: matches the
  old `.ok()?` shapes exactly. `Db::open` failure per line skips the line (old behavior).
- **Injection / path / permission**: argv is two static arrays (`events_argv`) plus
  `backend_prefix(backend)` (`["podman"]` / `["sudo","-n","podman"]`) — no untrusted string
  reaches `Command::new`. Event container names are filtered on `CONTAINER_PREFIX` and only
  ever used as lookup keys; the persisted worktree path comes from the DB, never from the
  event. Rootful `available()` probes the last prefix element (`podman`, not `sudo`); a
  passwordless-sudo failure degrades to a silent EOF + reap on the subscriber thread.
- **Races / lifecycle**: one `ContainerEvents` box per stream (subscribe consumes
  `Box<Self>`); no shared mutable state between the two threads; each event opens its own
  SQLite handle (old model). `proc_registry` registration held to scope end, reaped after
  `child.wait()`; the network child is now also registered (strictly better accounting, an
  accepted micro-delta). EOF → reap → thread exit; no zombie (the only theoretically
  unreaped path — `stdout.take()` failing — is unreachable after `Stdio::piped()` and is
  unchanged from the old code). No retry on daemon restart: pre-existing behavior, unchanged.
- **Off-loop / 0%-idle**: subscribe loops block dedicated Background-QoS threads (QoS first
  statement — `long_lived_threads_declare_a_qos_class` green); `spawn` is fire-and-forget;
  the loop only drains via `try_recv`. A hanging runtime cannot touch the loop.
- **Ratchets**: runtime-leak clean (7 pinned) and **negative control fires** (exit 1, tree
  restored) on the merged tree; forge-leak (4), ignored-result (326, after fix 3),
  async-trait (0) all clean; `justfile` lint + ratchet-update lines byte-consistent.
- **Chunk-2 findings (the lead's "4" — five listed)**: each explicitly flagged, none fixed
  here, matching the Lead's binding decision — `sandbox_compose.rs` IMPL, `placement.rs` /
  `sandbox_cpucap.rs` / `sandbox_dormant_tests.rs` IMPL fixtures, `sandbox_tests.rs` IMPL,
  and `agent.rs` + `vpn/mod.rs` LEAK-debt pinned with burn-down notes in the ratchet header.
  Verified the flagged sites still exist at the claimed lines on the merged tree.
- **Doctor enrichment (chunk 3)**: `with_caps` is applied over a `Null`-caps base
  (`Backend::probe` sets no caps) — nothing clobbered; the static-cap-before-classify shape
  makes every row describe its events cap on every path incl. `NotInstalled`, and the test
  asserts it over all of `Backend::ALL` hermetically.
- **Config visibility**: `select_backend` reads the global `cfg.sandbox` — the same
  visibility the old `network_audit` path had; per-repo/profile overlays resolve per
  worktree and the design's stated model is one global subscriber. Default auto chain leads
  with `podman-rootless`, so default behavior matches the old `have("podman")` gate. A host
  with an explicit non-podman global backend now (deliberately, §2.6) gets no events stream.

## Verification record (this lane, merged tree + fixes)

| Check                                                                                                                                   | Result   |
| --------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| `nextest -p thegn-core -E 'test(env_overlay) \| test(config_example) \| test(capability) \| test(seam)'` (MANDATORY)                    | 37/37 ✅ |
| `nextest -p thegn-host -E 'test(complete) \| test(help) \| test(catalog_tests) \| test(platform_ratchet) \| test(ratchet)'` (MANDATORY) | 92/92 ✅ |
| `nextest -p thegn-svc -E 'test(registry) \| test(conformance)'`                                                                         | 26/26 ✅ |
| `nextest -p thegn-core sandbox_events` (incl. new die-prune test)                                                                       | 14/14 ✅ |
| `nextest -p thegn-core --test sandbox_audit`                                                                                            | 3/3 ✅   |
| `nextest -p thegn-host sandbox_events`                                                                                                  | 4/4 ✅   |
| `nextest -p thegn-host -E 'test(pipeline_retry) \| test(retry)'` (fix 3)                                                                | 8/8 ✅   |
| `nextest -p thegn-host -E 'test(qos_class) \| test(long_lived)'`                                                                        | 1/1 ✅   |
| `cargo clippy -p thegn-core --lib` / `-p thegn-host --tests` / `-p thegn-host`                                                          | clean ✅ |
| `cargo fmt --check -p thegn-core -p thegn-host`                                                                                         | clean ✅ |
| ratchet.sh: runtime-leak / forge-leak / ignored-result / async-trait (+ negative control)                                               | clean ✅ |
| Host vendor exec/probe sweep (`Command::new("podman"\|"docker")`, `have(...)` shapes)                                                   | empty ✅ |

## Frame-affecting note (e2e)

No snapshot re-recording needed. The TUI frame surface is unchanged: on a runtime-less e2e
machine no subscriber threads spawn (`available()` gate, same as the old `have("podman")`
gate), the audit panel renders identically (no events → no rows), and the renamed threads
(`sandbox-events-exec`/`-net`) reach no rendered chrome (no e2e spec covers the Telemetry
overlay). `thegn doctor`'s text/`--json` gained the events notes — doctor is a CLI
subcommand and no muse spec drives it (verified: 0 matches in `test/muse/`). No e2e run
performed per the lead addenda.

## Non-blocking notes (watch items, for the record)

- Coverage: the defensive insert/prune-failure branches in `persist` remain unreachable
  in-memory (architect's watch item stands); `just ci`'s 95% gate owns the number.
- Micro-nit, not fixed: the podman transport opens the DB before `persist`'s prefix filter
  (old code filtered first). Only reachable for thegn-labeled containers with non-thegn
  names — effectively never, due to `--filter label=io.thegn=true`; one extra SQLite open on
  a non-hot path. Not worth churn on a verbatim-move branch.
- Pre-existing and unchanged (not introduced here): unbounded `lines()` line length on the
  vendor stream; no resubscribe after a podman daemon restart; audit rows trust thegn-labeled
  container names (local audit cache, same trust model as before THE-79).
- For the THE-86 lane: `pipeline_retry.rs`'s ignored-result shape was the lint blocker fixed
  in `e1d8e43c`; main's `just lint` is red without it (or a ratchet pin).
