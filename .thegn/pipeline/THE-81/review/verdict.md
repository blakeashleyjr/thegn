# THE-81 — Security / Test / Bug review verdict

**PASS**

Reviewer: security/test/bug lane · Branch `tg/the-81-ignored-result-audit` ·
Reviewed at `7ff81214` (post architect-review commits, post `git merge main`).

## Scope & method

The branch is a ~2000-site sweep over the ignored-result allowlist: 317 files,
+3805/−1621. The danger for this lane is inverted: a load-bearing `let _ =`
(waker pulse, send-to-gone-consumer, DB cache write, cleanup, Drop teardown)
turned into a surfaced error, early return, or control-flow change on the loop,
in the pty drain, in the daemon service, or in a Drop.

To make that checkable I built a **comment-stripped, order-sensitive semantic
diff** of `git diff main...HEAD -- crates/` (naive comment stripping defeats
the annotation-only bulk; sequence comparison rather than multiset defeats
silent reordering). Result: **29 files carry real code changes**; every other
touched file is annotation-only. All 29 were read line-by-line; a weighted
sample of ~55 annotation-only sites across `run.rs`, `handlers/*`,
`daemon/*`, `pty_drain.rs`, `pane_pty.rs`, `panes.rs`, `bridge/mod.rs` and the
DB layer was verified to keep identical statement text with only the comment
appended.

## Findings — the risk surface is clean

1. **No control-flow changes.** The only (b)-rewrites are shape-preserving:
   - `db_pool.rs` ×4, `db_hibernate.rs`, `db_iroh.rs` (token auth — still
     **fails closed**), `db_workspace.rs` ×2: `.ok()` → match that keeps
     `QueryReturnedNoRows → None` (miss semantics identical; zero behaviour
     change on the empty path) and adds `tracing::warn!` on real errors.
   - `config.rs`: 19 env-enum fallbacks now go through `parse_enum_env` —
     fallback-to-default semantics unchanged, invalid value warns via
     `config_warn` (parity with the sibling float parser). All 14 mandated
     `env_overlay`/`config_example`/`control_schema` tests pass.
   - `actions.rs` PR fetch, `run.rs` delete-flow `Db::open`, `doctor.rs`
     hosts report, `media_ctl.rs`, `handlers/pr_queue.rs`, `daemon/mod.rs`
     axum serve, `forward.rs`/`bridge_sup.rs` thread spawns, `hydrate.rs`
     move-on-merge, `dns_filter.rs` listener, `svc/bridge` exec-worker
     (now answers `resp_err` instead of hanging the request), `svc/host`
     volume-seed rollback: every one keeps the original `None`/degrade
     outcome and only adds a warn/status/`outln`. No early returns added,
     no signatures changed, no new unwraps.
2. **No new wake sources.** 149 diff lines mention `.wake()`; every one is a
   pre-existing statement with a comment appended (order-sensitive diff shows
   zero inserted wake lines). The 0%-idle contract is untouched — confirmed
   by the render-plan suite.
3. **No blocking calls added to the loop.** All `.join()`/`block_on` lines in
   the diff are pre-existing, annotation-only, and off-loop (test scaffolding
   or thread teardown).
4. **Drops intact.** `BridgeClient::drop` (kill/wait), `host_flow`-adjacent
   child reaping, tmp-dir cleanup in tests: statements unchanged, annotated
   only.
5. **Allowlist discipline verified.** `test/ignored-result-ratchet.txt`:
   **deletions only** — 10 files released (`calendar/recur.rs`,
   `db_hibernate.rs`, `db_pool.rs`, `forge/mod.rs`, `panel/gitfull.rs`,
   `panel/sections/misc.rs`, `svc/ci.rs`, `svc/control/mod.rs`,
   `svc/seam/registry.rs`, `thegn-media/mpd.rs`), zero additions. Count
   reconciles: design measured 325 at `d361b60a`; main's later merges added a
   326th pin; 326 − 10 = **316 pinned**, matching the header update (the one
   hunk chunk 3 reserved). `bash test/ratchet.sh ignored-result …` (the exact
   `just lint` gate) → **clean (316 pinned)**.
6. **Ratchet/architecture gates:** core 14/14 (incl. `platform_cfgs_are_pinned`,
   host-key-literal chokepoint); host 114/114 incl. `render_plan` (20/20),
   help ratchet, `catalog_tests`, `platform_ratchet`; `cargo clippy -p
thegn-host --tests` clean — this closes chunk-2's "test-target clippy not
   run" unverified item.

## Non-blocking observations (for the record; no action required to merge)

- **`db_migrate.rs` class note stands** (55 annotated sites): a non
  already-applied `ALTER` failure would be skipped while the version advances.
  Pre-existing, correctly left for a follow-up ticket rather than a 55-site
  rewrite inside this change.
- **`ssh_creds.rs`** warn-only fix accepted: the 0600 tighten failure now logs,
  but the state dir is still created 0755. A 0700 `create_dir_all` would be
  cheap insurance in a follow-up. Content is config, not keys.
- **(c) flags** (`doctor` merge-guard JSON `"audit": null`, `panes.rs:1527`
  drawer spawn, `secret.rs:533`, MCP initialized-ack, cloud-init wait,
  `send_input`/`resize` backchannel): code untouched this branch, annotated
  with honest whys — correctly deferred to dedicated changes.
- **Coverage note for the fold:** the (b) warn branches need fault injection
  to hit; the `NoRows → None` arms are exercised by existing miss tests. If
  the 95% core gate moves at `just coverage`, a `cov_ignore` entry or a
  fault-injected test is the fix — not a gate hack.
- The 6 "accepted flags" and every coder "Unverified" item from the chunk
  reports were checked: all are either verified green above (test-target
  clippy) or explicitly out of this change's containment (e2e, cross-compile,
  full `just ci` — pre-push/fold territory per policy).

## e2e / frames

**No snapshot re-recording needed.** No render-path file carries a semantic
change; the two released panel files changed only by param/binding renames
(behaviour-neutral, verified in the order-sensitive diff). Frame output is
byte-identical.

## Commands run (scoped per dev-loop policy)

- `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example)
| test(control_schema) | test(ratchet)'` → **14/14 passed**
- `cargo nextest run -p thegn-host -E 'test(complete) | test(help) |
test(catalog_tests) | test(platform_ratchet) | test(ratchet) |
test(render_plan)'` → **114/114 passed** (render_plan filter separately
  re-confirmed: 20/20)
- `cargo clippy -p thegn-host --tests` → clean
- `bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|
\.ok\(\);' crates` → clean (316 pinned)
- Main is fully contained in HEAD (`git rev-list --count HEAD..main` = 0) —
  the required `git merge main` was already performed (`b00af1e1`) and its
  three conflicts were resolved as documented in the architect verdict.

No code fixes were required by this review; the two architect fixes
(`parse_enum_env`, ssh_creds perms warn) were re-verified independently.
Ready for `thegn integrate`.
