# Tasks

## 1. Runtime / database compatibility

- [x] 1.1 `db::schema_refusal` — pure policy refusing a build older than the
      on-disk schema, with `THEGN_ALLOW_SCHEMA_DOWNGRADE` as the escape hatch.
- [x] 1.2 One actionable error replacing the repeated warning; the tolerant
      read-only path survives only behind the override.
- [x] 1.3 `Db::open_at_allowing_older_build` so the tolerant branch stays
      testable without mutating process-global environment.
- [x] 1.4 Tests: old-runtime/new-database refusal (pure + end-to-end over a
      real file), and the override still reaching the tolerant path.
- [x] 1.5 `thegn doctor` prints the schema pair and both the CLI's and each
      daemon's actual binary — what `--version` could not distinguish.
- [x] 1.6 Document the dev-shell `target/debug` vs `just live` `target/release`
      split in `flake.nix`, the deployment defect behind the incident.

## 2. Dispatch de-duplication and leases

- [x] 2.1 `pipeline_claim` — pure decide(rows, request, limit).
- [x] 2.2 Identity keyed on issue + stage + worktree + artifact + chunk, so
      legitimate parallel chunks are not refused.
- [x] 2.3 `db::claim_dispatch` runs the policy inside `BEGIN IMMEDIATE`.
- [x] 2.4 `--allow-duplicate <reason>` override, its note committed in the same
      transaction as the row.
- [x] 2.5 `pipeline_leases` table + acquire/release/holder, owner-scoped, with
      expiry so a crashed holder does not wedge the pipeline.
- [x] 2.6 `thegn dispatch claim` / `thegn dispatch lease` CLI verbs, classified
      in the completion catalog.

## 3. Exit reconciliation

- [x] 3.1 Schema v63: `agent_dispatches.exit_code` / `.exited_at_ms`.
- [x] 3.2 `stamp_dispatch_exit`, called on pane exit even when the row's status
      deliberately does not move.
- [x] 3.3 `pipeline_run::row_liveness` → `Live | ExitedUnverified | Closed`,
      with absence of a stamp meaning unknown, never exited.
- [x] 3.4 `dispatch list` prints `running!exited` plus a footer stating those
      rows are not free capacity; `--json` carries a `liveness` field.
- [x] 3.5 Tests for each arm, including the monitor-restart case.

## 4. Concurrency enforcement

- [x] 4.1 Per-stage capacity counted from rows, exited-but-unclosed included.
- [x] 4.2 Budget read from the stage's configured `concurrency`.
- [x] 4.3 Tests: concurrent claim attempts, monitor restart, terminal rows
      freeing their slot.

## 5. Pipeline contract validation

- [x] 5.1 `stage_contract_gaps` / `validate_stage_contracts`, placeholder-aware.
- [x] 5.2 Wired into `config validate` (error) and load-time warnings.
- [x] 5.3 Tests, including the escaped-brace case.
- [ ] 5.4 **Not done:** an end-to-end test driving dispatch → worker report →
      artifact verification → done. The pieces are unit-tested in isolation;
      the full loop needs a worktree fixture with a git-tracked artifact and is
      better placed in `test/smoke.sh` than in a unit test.

## 6. Disk-pressure protection

- [x] 6.1 `RECLAIM_COOLDOWN_SECS` per-worktree hysteresis.
- [x] 6.2 `LOW_DISK_OVERSHOOT_PCT` — evict past the warn line, not onto it.
- [x] 6.3 `awaiting_verification` exemption, fed by
      `worktrees_with_active_dispatch`.
- [x] 6.4 Tests for the reclaim/build race and both hysteresis halves.
- [ ] 6.5 **Not done:** collapsing repeated disk-pressure warnings into one
      summarized event. The reclaim itself already emits a single
      `disk_cleaned` notification per worktree; the noisy path was the schema
      warning, fixed separately in `4eba4407`.

## 7. Logging and observability

- [x] 7.1 Schema-mismatch logging is now a single hard error (or one warning
      under the override) rather than a per-open warning.
- [x] 7.2 `thegn doctor` surfaces the schema pair and binary parity.
- [ ] 7.3 **Not done:** metrics for active rows / live sessions /
      exited-unverified / duplicate refusals / reclaim volume / monitor
      ownership. `dispatch list` reports the stale count on demand; a metrics
      surface is a separate change.

## 8. life-automation integration

- [x] 8.1 Audited: **thegn is not deployed through Nix at all** — no package,
      no systemd unit, no home-manager module. It runs from a checkout via
      `just live`, and its daemon is a child of that process. There is no
      deployment to fix; the defect is the dev shell's `PATH`, documented in
      1.6.
- [x] 8.2 `thegn.slice` limits are applied at runtime by `systemd-run`
      (`user.control` drop-ins), not declaratively — confirmed capped at
      22 CPU-s/s and MemoryHigh 56 GiB during the incident.
- [ ] 8.3 **Not done:** a deployment smoke check. It presupposes a deployment;
      the equivalent guard here is `thegn doctor`'s parity block.
- [ ] 8.4 **Not done:** reviewing backup / Btrfs / Podman / indexing services
      for interaction with worktrees. Note for whoever picks it up: the root
      filesystem is ext4-on-LUKS (not Btrfs), and `nix/common-modules/services/
disk-space-monitor.nix` has uncommitted local edits that should be
      resolved first. zram is configured in `ws-modules/ws-memory.nix` and is
      **not** the disk-I/O root cause — the 386 GiB of `target/` was.

## 9. Validation

- [x] 9.1 `just test` — 7134 passed.
- [x] 9.2 `cargo clippy --workspace --all-targets -D warnings` — clean.
- [x] 9.3 `just smoke` + PTY smoke — passed.
- [x] 9.4 `treefmt --ci` — clean.
- [ ] 9.5 `just lint` fails on a **pre-existing** `json-emit` ratchet violation
      in `crates/thegn-host/src/cmd/session_fork.rs`, reproduced on a clean
      stash of this branch. Unrelated to this change; fold-landing debt.
