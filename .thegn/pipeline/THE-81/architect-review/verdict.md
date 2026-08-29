# THE-81 — Architect review verdict

**VERDICT: APPROVED**

Reviewed: full branch diff `git diff main...HEAD` (branch tip `41611ffd`, after
the required `git merge main` + two architect commits), all three chunk specs +
done reports, every "Unverified" section, and the design
(`architect/design.md`) against repo standards (CLAUDE.md ignored-Result rule,
ARCHITECTURE.md §9, the ratchet mechanics in `test/ratchet.sh` / `justfile`).

## Merge (LEAD addendum)

`git merge main` landed as `b00af1e1`. Three content conflicts — all the same
shape (main's THE-70/THE-79/THE-84 refactors touched lines this branch had
annotated):

- `notify.rs` — main added QoS around the sound spawn; kept main's QoS code +
  the branch's inline `// best-effort:` on the `let _ =`.
- `run.rs` — main replaced `launch_spec(.., "shell")` with
  `prewarm_spec` (THE-84); kept main's call and re-applied the best-effort
  annotation to the surviving `let _ =`.
- `sandbox_events.rs` — main's podman seam deleted every function the branch
  had annotated (`subscribe_exec`/`subscribe_network`/…); took main's side
  wholesale and re-annotated the one new unannotated spawn `.ok()` in
  `subscribe_thread` (main's own `BatchForwarder` send already carried the
  comment).

Post-merge ratchet: **clean (316 pinned)**.

## Verification performed (all green)

- `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example)
| test(control_schema) | test(ratchet)'` → 14/14.
- `cargo nextest run -p thegn-host -E 'test(complete) | test(help) |
test(catalog_tests) | test(platform_ratchet) | test(ratchet)'` → 94/94.
- Extra (architect diligence, my config.rs fix): `cargo nextest run -p
thegn-core config::` → 247/247.
- `just quick` on every touched crate — thegn-core, thegn-host, thegn-svc,
  thegn-media, thegn-metrics, thegn-proxy, gtui-app, tg-kit — clippy
  `-D warnings` clean.
- `bash test/ratchet.sh ignored-result '…' crates` (the exact `just lint`
  gate) → clean, 316 pinned, both after the merge and after my commits.
- Independent site census: 2042 matching sites at base → 1958 now. Sampled
  ~40 residual sites across the "no token within ±1 line" set: all carry
  either the annotation 2 lines above (builder chains) or the documented
  pre-existing prose blocks (yazi, db.rs, remote.rs, profile.rs, placement.rs
  …). No silently-skipped sites found. The chunk counts reconcile with the
  observed diff.

## Design conformance

- **Rubric applied correctly.** All 27 (b) rewrites reviewed line-by-line:
  semantics preserved everywhere (`QueryReturnedNoRows → None` miss shape
  kept; token verify fails _closed_; delete/queue flows keep their
  fall-back-and-warn shape); primary paths now surface (pr_queue status
  strings, doctor `outln`, panel/media/issue warns, bridge `resp_err` instead
  of a hung request). No signature changes, no new unwraps, no e2e-visible
  frame changes — containment respected.
- **Allowlist discipline** is exactly per the gate's true mechanics: 12 files
  released, all of which now have zero non-comment matches (ratchet clean
  proves it); annotation-only files correctly stay pinned; header prose
  corrected as the design reserved for chunk 3. Deletions only.
- **"Unverified" items verified by me:** test-target compilation (nextest ran
  the `#[cfg(test)]` code: pass); scoped-ratchet noise explained in chunk-1 is
  real (global allowlist vs scoped grep) and the full run is the authoritative
  one — it is clean; treefmt applied at commit; clippy on all touched crates
  clean.
- **Load-bearing (a) claims spot-verified in code:** db_cache "failed read is
  a miss; the live source repopulates" (refresh cycle independent of cache
  reads); hydrate watcher "2s safety-net ticker covers a missed root" (in-code
  comment + fallback logic); host_flow run_effect "outcome advisory" (verified:
  `run_effect` publishes failures through the step board/`publish` as it runs,
  so discarding the returned `Option<DriveFlow>` on drain paths does not hide
  the failure — though the annotation's "re-runs on next reconcile" tail is
  generous; accepted as harmless optimism).

## Architect corrections applied (committed `41611ffd`)

Two of chunk-1's (c) flags were cheap, in-design fixes, so I made them rather
than bouncing the branch:

1. **config.rs — 19 env-override enum fallbacks** (`THEGN_PICKER=bogus` etc.
   silently kept the default). The design's A5 rule was "annotate iff `config
validate` already reports the invalid value" — it did not. Added
   `parse_enum_env` (parity with the sibling `parse_float`) so an invalid
   value warns via `config_warn` — the helper whose own doc says the env layer
   should speak with it — and still falls back. Fallback semantics unchanged;
   the existing `env_overlay_bad_enum_values_yield_none` test still passes.
2. **ssh_creds.rs — failed `0600` tighten** now warns instead of silently
   leaving the flattened ssh_config world-readable under the default-0755
   state dir (small but real hygiene gap).

## Accepted flags (documented in the done reports; non-blocking)

- `db_migrate.rs` class note (55 sites; an `ALTER` failing for a non
  already-applied reason would be skipped while the version advances) — real,
  pre-existing, 55-site rewrite exceeds containment; candidate follow-up
  ticket.
- `cmd/doctor.rs` merge-guard JSON emits `"audit": null` without a reason —
  minor; JSON consumers only.
- `panes.rs:1527` silent drawer spawn — systemic UX decision shared with
  sibling spawn helpers; correctly left for a dedicated change.
- `secret.rs:533` idempotent-rm semantics — defensible as-is.
- MCP initialized-ack, cloud-init wait, send_input/resize backchannel —
  annotated with honest whys; revisit if those surfaces gain error channels.

## Notes for the fold

- **Coverage gate (CI):** the (b) rewrites add warn arms that only fault
  injection reaches (chunk-1 flagged this honestly). The `NoRows → None` arms
  ARE exercised by existing miss tests. If `just coverage`'s 95% core gate
  moves, these few warn lines are where to look — a `cov_ignore` entry or a
  fault-injected test, not a gate hack, is the fix.
- Cross-platform cfg'd code (SMTC/taskkill/DACL stubs) typechecks via clippy
  on Linux only; unchanged by this branch beyond comments/param renames, so
  exposure is minimal. `just check-cross` at fold will confirm.
- Pre-push remains the single heavy gate: `just test` (nextest full) +
  clippy + smoke, then `just ci` before PR per policy.
