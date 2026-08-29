# THE-81 chunk 2 — Audit every ignored Result in `thegn-host`

**Read `.thegn/pipeline/THE-81/architect/design.md` first** — the classification
rubric, the handling idioms, and the containment rules there are binding for
this chunk. This file is the executable subset for the host crate. This is the
largest chunk (183 files, 1144 sites); the rubric exists so the ≈1122
mechanical sites (waker pulses, channel sends, cache writes, teardown) are
annotated fast and the deep reading goes to the ambiguous shapes.

Goal: for every site in the pinned files below that matches
`let _ = `, `let _ =$` (rustfmt-wrapped), or `.ok();`, either (a) annotate the
sanctioned best-effort ones `// best-effort: <why>`, (b) surface the
primary-path ones (then delete the file's line from the allowlist), or (c)
leave untouched and list it for the reviewer.

## Files touched (exact)

Code scope — every pinned file under `crates/thegn-host/` (183 files at base
`d361b60a`; the list below is the authority, taken from
`test/ignored-result-ratchet.txt`):

```
crates/thegn-host/examples/waker_spike.rs
crates/thegn-host/src/actions.rs
crates/thegn-host/src/agent_configs.rs
crates/thegn-host/src/agent_home.rs
crates/thegn-host/src/agent.rs
crates/thegn-host/src/agent_run.rs
crates/thegn-host/src/agent_tests.rs
crates/thegn-host/src/apps/mod.rs
crates/thegn-host/src/attention_status.rs
crates/thegn-host/src/autoscale.rs
crates/thegn-host/src/blast_radius.rs
crates/thegn-host/src/bridge_sup.rs
crates/thegn-host/src/build_cache.rs
crates/thegn-host/src/ci_refresh.rs
crates/thegn-host/src/cli_help.rs
crates/thegn-host/src/clipboard.rs
crates/thegn-host/src/cmd/attach.rs
crates/thegn-host/src/cmd/ci.rs
crates/thegn-host/src/cmd/config.rs
crates/thegn-host/src/cmd/diff.rs
crates/thegn-host/src/cmd/disk.rs
crates/thegn-host/src/cmd/doctor.rs
crates/thegn-host/src/cmd/host.rs
crates/thegn-host/src/cmd/integrate.rs
crates/thegn-host/src/cmd/merge.rs
crates/thegn-host/src/cmd/mod.rs
crates/thegn-host/src/cmd/open.rs
crates/thegn-host/src/cmd/pr_queue.rs
crates/thegn-host/src/cmd/pr.rs
crates/thegn-host/src/cmd/session.rs
crates/thegn-host/src/cmd/share.rs
crates/thegn-host/src/cmd/theme.rs
crates/thegn-host/src/cmd/wt.rs
crates/thegn-host/src/config_source.rs
crates/thegn-host/src/connectivity_gate.rs
crates/thegn-host/src/daemon/client.rs
crates/thegn-host/src/daemon/inbox.rs
crates/thegn-host/src/daemon/mod.rs
crates/thegn-host/src/daemon/record.rs
crates/thegn-host/src/daemon/service.rs
crates/thegn-host/src/daemon/session.rs
crates/thegn-host/src/db_task.rs
crates/thegn-host/src/desktop_notify.rs
crates/thegn-host/src/detail/usage_dash.rs
crates/thegn-host/src/drawer_state.rs
crates/thegn-host/src/emulator.rs
crates/thegn-host/src/env_ui.rs
crates/thegn-host/src/fff_backend.rs
crates/thegn-host/src/font.rs
crates/thegn-host/src/forge_handle.rs
crates/thegn-host/src/forward.rs
crates/thegn-host/src/frame_writer.rs
crates/thegn-host/src/frame_write.rs
crates/thegn-host/src/git_handle.rs
crates/thegn-host/src/gitmut.rs
crates/thegn-host/src/git_watch.rs
crates/thegn-host/src/handlers/attention.rs
crates/thegn-host/src/handlers/calendar.rs
crates/thegn-host/src/handlers/creating.rs
crates/thegn-host/src/handlers/daemon_lifecycle.rs
crates/thegn-host/src/handlers/host_heal.rs
crates/thegn-host/src/handlers/host.rs
crates/thegn-host/src/handlers/launch.rs
crates/thegn-host/src/handlers/materialize.rs
crates/thegn-host/src/handlers/merge_queue.rs
crates/thegn-host/src/handlers/paste_image.rs
crates/thegn-host/src/handlers/pr_queue.rs
crates/thegn-host/src/handlers/provision.rs
crates/thegn-host/src/handlers/sidebar_keys.rs
crates/thegn-host/src/handlers/sidebar_persist.rs
crates/thegn-host/src/handlers/startup.rs
crates/thegn-host/src/handlers/tracker.rs
crates/thegn-host/src/handlers/worktree_delete.rs
crates/thegn-host/src/handlers/worktree_rename.rs
crates/thegn-host/src/handlers/workspace_remove.rs
crates/thegn-host/src/hibernator.rs
crates/thegn-host/src/host_flow.rs
crates/thegn-host/src/host_provision.rs
crates/thegn-host/src/hydrate_calendar.rs
crates/thegn-host/src/hydrate_tests.rs
crates/thegn-host/src/hydrate.rs
crates/thegn-host/src/integrate.rs
crates/thegn-host/src/loc_scan.rs
crates/thegn-host/src/machine0_bridge.rs
crates/thegn-host/src/main.rs
crates/thegn-host/src/managed_tool.rs
crates/thegn-host/src/mcp_proxy/upstream.rs
crates/thegn-host/src/measure/disk.rs
crates/thegn-host/src/measure/loc.rs
crates/thegn-host/src/measure/mod.rs
crates/thegn-host/src/media_art.rs
crates/thegn-host/src/media_ctl.rs
crates/thegn-host/src/media_overlay.rs
crates/thegn-host/src/media_watch.rs
crates/thegn-host/src/menu.rs
crates/thegn-host/src/merge_driver.rs
crates/thegn-host/src/merge_lifecycle.rs
crates/thegn-host/src/merge_remote.rs
crates/thegn-host/src/metrics.rs
crates/thegn-host/src/model_proxy_daemon.rs
crates/thegn-host/src/monitor_action.rs
crates/thegn-host/src/monitor.rs
crates/thegn-host/src/monitor/state.rs
crates/thegn-host/src/mq_assets.rs
crates/thegn-host/src/notify.rs
crates/thegn-host/src/palette.rs
crates/thegn-host/src/panel/gitfull.rs
crates/thegn-host/src/panel/sections/misc.rs
crates/thegn-host/src/panel_util.rs
crates/thegn-host/src/pane_pty.rs
crates/thegn-host/src/pane.rs
crates/thegn-host/src/panes.rs
crates/thegn-host/src/pane_writer.rs
crates/thegn-host/src/parity.rs
crates/thegn-host/src/perf.rs
crates/thegn-host/src/placement_flow.rs
crates/thegn-host/src/platform/proc.rs
crates/thegn-host/src/platform/unix.rs
crates/thegn-host/src/platform/windows.rs
crates/thegn-host/src/plugins.rs
crates/thegn-host/src/pr_driver.rs
crates/thegn-host/src/preview_gfx.rs
crates/thegn-host/src/preview_pane.rs
crates/thegn-host/src/probe.rs
crates/thegn-host/src/profile.rs
crates/thegn-host/src/provider_workdir.rs
crates/thegn-host/src/provision_gate.rs
crates/thegn-host/src/pty_drain.rs
crates/thegn-host/src/push_notify.rs
crates/thegn-host/src/queries.rs
crates/thegn-host/src/rasterize.rs
crates/thegn-host/src/remote_poll.rs
crates/thegn-host/src/remote_sync.rs
crates/thegn-host/src/replay_overlay.rs
crates/thegn-host/src/repo_index.rs
crates/thegn-host/src/run.rs
crates/thegn-host/src/run_tests.rs
crates/thegn-host/src/sandbox_events.rs
crates/thegn-host/src/search_apply.rs
crates/thegn-host/src/search_everywhere.rs
crates/thegn-host/src/search_overlay.rs
crates/thegn-host/src/search.rs
crates/thegn-host/src/search_worker.rs
crates/thegn-host/src/secret.rs
crates/thegn-host/src/session.rs
crates/thegn-host/src/share.rs
crates/thegn-host/src/sidebar.rs
crates/thegn-host/src/sprite_bridge.rs
crates/thegn-host/src/structural_diff.rs
crates/thegn-host/src/task.rs
crates/thegn-host/src/telemetry.rs
crates/thegn-host/src/testkit/report.rs
crates/thegn-host/src/vps_reaper.rs
crates/thegn-host/src/wire.rs
crates/thegn-host/src/wizard.rs
crates/thegn-host/src/workspace_create.rs
crates/thegn-host/src/workspace_picker.rs
```

Plus exactly two audit files:

- `test/ignored-result-ratchet.txt` — **delete the lines of files you
  released** (zero non-comment matches left). Deletions only; never add a
  line; never run `RATCHET_UPDATE=1`; never touch the `#` header (chunk 3
  owns the header prose).
- `.thegn/pipeline/THE-81/code/chunk-2-done.md` — the report (format below).

## Overlap / dependency

**None — fully file-disjoint from chunks 1 and 3; the Lead may run all three
in parallel.** The shared allowlist is safe: you delete only
`^crates/thegn-host/` lines. Do not edit `justfile`, `test/ratchet.sh`, or
anything outside the two scopes above. If run serially, this chunk goes
**second**.

## Approach (per-site loop)

Same loop as chunk 1, with the host's confirmed hot spots. Evidence sampled at
base (verified this turn, not hypothesized):

| Hot file (sites)                                                                                                                                                                                                                                                                                                                 | Dominant shapes                                                                                                                                                                                                                                                                                              | Provisional class                                                                                                                                                             | Watch for                                                                                                                                                                                                                                                                                               |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `run.rs` (126)                                                                                                                                                                                                                                                                                                                   | waker pulses (:326, :874, :1906, :2171, :2224, :2274, :2477, :2576-:2687), `tx.send` to off-loop workers (:850, :2170, :2223, :2273), DB cache writes (:1610, :1613, :1674-:1680, :2074, :2107-:2110, :1942, :1124), register writes (:220-:223), terminal teardown (:1100-:1111), fs cleanup (:1893, :1913) | (a) overwhelmingly                                                                                                                                                            | **`run.rs:1821`** `Db::open().ok()` in the delete flow — the comment right below explains DB-resolved env prevents a provider-sprite leak; a silent `None` degrades teardown → (b): `match` with `tracing::warn!`, no behaviour change beyond the warn                                                  |
| `hydrate.rs` (45)                                                                                                                                                                                                                                                                                                                | waker/send (:511, :611, :650, :877), DB cache writes (:979-:985, :1109-:1166), `env::var("THEGN_SESSION").ok()` (:1015)                                                                                                                                                                                      | (a)                                                                                                                                                                           | `:1109` `prune_stale_worktree_groups` — check whether its `Result` carries user-facing report info before annotating; if it does → (b)/(c)                                                                                                                                                              |
| `merge_lifecycle.rs` (34), `host_flow.rs` (34), `task.rs` (26), `integrate.rs` (18), `merge_driver.rs` (17)                                                                                                                                                                                                                      | queue/state cache writes, waker, sends                                                                                                                                                                                                                                                                       | (a) mostly                                                                                                                                                                    | any cache write whose failure leaves the queue _display_ contradicting git/forge state without a log → (b) via `tracing::warn!`                                                                                                                                                                         |
| `actions.rs` (21)                                                                                                                                                                                                                                                                                                                | UI action paths                                                                                                                                                                                                                                                                                              | mixed                                                                                                                                                                         | **`actions.rs:725-726`** `forge.conversation(...).ok()` / `forge.pr_diff(...).ok()` — the PR view panel opens silently empty on any forge/network error → (b): surface (idiom 2 or 3; e.g. warn + status message). This is the flagship class-(b) site of the chunk                                     |
| `cmd/*` (18 files)                                                                                                                                                                                                                                                                                                               | DB cache writes (`cmd/ci.rs:173,184`, `cmd/merge.rs:398,561-596`, `cmd/host.rs:127,260,265`, `cmd/pr.rs:181`), already-annotated (`cmd/open.rs:64-65`, `cmd/session.rs:903-908`), cleanup (`cmd/config.rs:89-92,499`)                                                                                        | (a)                                                                                                                                                                           | **`cmd/doctor.rs:1432`** `Db::open().ok()` — doctor's Hosts section renders "(none)"-ish state with no db-unavailable reason → (b): print/warn the open error. `cmd/diff.rs:89` stdout `write_all` → (a): EPIPE on a closed `head` pipe is normal. `cmd/session.rs:908` already fully annotated — no-op |
| `handlers/*` (17 files)                                                                                                                                                                                                                                                                                                          | DB cache writes (`workspace_remove.rs:208-241`, `worktree_delete.rs`, `merge_queue.rs`, `sidebar_persist.rs`), waker, cleanup                                                                                                                                                                                | (a)                                                                                                                                                                           | user-invoked handler paths: if the swallowed error changes what the user sees with no message → (b) via `model.status`/`msg!`                                                                                                                                                                           |
| `Db::open().ok()` bindings across the crate (`agent.rs:3052`, `handlers/merge_queue.rs:941`, `handlers/workspace_remove.rs:91`, `handlers/worktree_delete.rs:114`, `hydrate_calendar.rs:57`, `provision_gate.rs:186`, `pty_drain.rs:68`, `run.rs:729`, `run.rs:15851`, `wizard.rs:131`, `cmd/share.rs:86`, `cmd/doctor.rs:1432`) | silent DB-less fallback                                                                                                                                                                                                                                                                                      | judge each: background/cache-only consumer → (a) annotate "best-effort cache"; user-visible surface degrades silently → (b) warn and rewrite so the pattern no longer matches | —                                                                                                                                                                                                                                                                                                       |
| `daemon/*`, `agent*.rs`, `search_worker.rs`, `media_watch.rs`, `bridge_sup.rs`, `sandbox_events.rs`, `vps_reaper.rs`                                                                                                                                                                                                             | off-loop workers: sends, waker, cache writes, teardown                                                                                                                                                                                                                                                       | (a)                                                                                                                                                                           | worker _outcome_ errors already surfaced elsewhere; don't double-log                                                                                                                                                                                                                                    |
| `run_tests.rs` (26), `hydrate_tests.rs` (26), `agent_tests.rs`, `testkit/report.rs`, `examples/waker_spike.rs`                                                                                                                                                                                                                   | test scratch teardown                                                                                                                                                                                                                                                                                        | (a) annotate                                                                                                                                                                  | setup-masking ignores → (c)                                                                                                                                                                                                                                                                             |

Release rule reminder: **an annotation never releases a file** — only sites
handled so the pattern no longer matches do (`match`/`if let Ok`, `?`, or
`drop(...)` for non-Result discards like `cmd/disk.rs:178`; **not**
`.inspect_err(…).ok()`). After handling a file's last matching site, delete
its line from the allowlist. `cmd/search.rs` (released by `cb775a7b`) is the
house precedent for a released CLI file.

## Tests to run (scoped; no full-workspace builds)

1. `just quick thegn-host` — clippy on lib/bin must stay clean.
2. `cargo nextest run -p thegn-host` — crate suite (sweep spans the crate;
   the help/keymap ratchet tests live here and must stay green).
3. `bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates/thegn-host`
   — must print `clean` with your deletions applied.

## Done-criteria

1. Every in-scope site is annotated (a), handled (b), or listed (c) — spot
   check: `git grep -nE 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' -- crates/thegn-host | grep -v 'best-effort'`
   returns only (b)-rewritten sites, (c) sites you list, and comment-only lines.
2. All three test commands above green.
3. `test/ignored-result-ratchet.txt` contains strictly fewer `^crates/thegn-host/`
   lines than at base (deleted exactly the released files), nothing added.
4. `chunk-2-done.md` written with: a per-file table (file → sites → a/b/c
   counts → action taken), the **Unsure — for the reviewer** list (file:line +
   the question), and the ratchet command output snippet.
5. Commit the chunk with subject exactly:

   `fix(the-81): audit thegn-host ignored-results — annotate best-effort, surface primary-path errors`
