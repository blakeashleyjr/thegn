# THE-81 chunk 2 — thegn-host ignored-result audit: completion report

Branch `tg/the-81-ignored-result-audit`, code commit (this change) —
`fix(the-81): audit thegn-host ignored-results — annotate best-effort, surface primary-path errors`.
Scope per `chunk-2.md`: all 183 pinned files under `crates/thegn-host/` (1144
matching sites at base `d361b60a`), plus surgical deletions in
`test/ignored-result-ratchet.txt` (2 lines, deletions only; header untouched).
Rubric and containment rules from `.thegn/pipeline/THE-81/architect/design.md`
were followed: (a) is comments-only (`// best-effort: <why>` trailing on
one-line lets, above the statement otherwise — no code movement); (b) edits are
limited to the ignore site + its immediate receiver (log line / status string /
doc-comment), no signature changes, no new unwraps; (c) sites are untouched and
listed below. Default bias (torn between (a) and (b) → (c)) was applied.

## Result summary

- **1144 sites audited** across 183 files (site = non-comment line matching
  `let _ = ` / `let _ =$` / `.ok();` at base).
- **(a) annotated this chunk: 976** — `// best-effort: <category>: <site-specific why>`
  added (waker pulses, sends to possibly-gone consumers, DB cache writes,
  cleanup/teardown, dir prep, optional inputs, first-set-wins statics, infallible
  String/Vec writes, child reaping, test-scaffolding effect calls).
- **(pre) already carried best-effort prose at base: 130** — left untouched per
  the design ("do not mass-rewrite existing prose"). 121 have the prose attached
  to the statement (e.g. `cmd/open.rs`, `daemon/inbox.rs:242`,
  `handlers/attention.rs` siblings); 11 have it 2–3 lines above
  (`cmd/attach.rs:193`, `config_source.rs:45`, `daemon/mod.rs:174`,
  `drawer_state.rs:271`, `handlers/paste_image.rs:138`, `handlers/pr_queue.rs:525`,
  `handlers/startup.rs:117/122`, `host_ui.rs:306`, `secret.rs:333`,
  `handlers/sidebar_persist.rs:147`).
- **(b) surfaced: 16 sites** (14 now stop matching the gate pattern; the 2
  thread-spawn sites use the in-tree `.inspect_err(warn).ok()` idiom plus an
  annotation — the design's non-release rule respected):
  - `actions.rs:725-726` (flagship): `spawn_pr_view_fetch`'s
    `forge.conversation(..).ok()` / `forge.pr_diff(..).ok()` → `match` +
    `tracing::warn!(target: "thegn::panel", …)`; the view still degrades to its
    half-empty state (documented in the doc comment) but the reason is logged.
    Threading the error into `PrViewData` would have been a type change beyond
    one hop → idiom 2 chosen, per containment.
  - `run.rs:1821`: delete-flow `Db::open().ok()` → `match` that keeps `None`
    semantics and warns (`thegn::worktree`, "sandbox teardown may be degraded").
  - `cmd/doctor.rs:1432`: doctor Hosts `Db::open().ok()` → `match` + `outln!`
    "(db unavailable: {e} — host sections below may be incomplete)".
  - `daemon/mod.rs:394`: the control-plane TCP `axum::serve` task now warns if
    the server exits (`thegn::daemon`).
  - `bridge_sup.rs:133`, `forward.rs:373`: fs-watch pump / forward-detector
    thread spawns warn on spawn failure (house idiom, cf. `model_proxy_daemon.rs`).
  - `hydrate.rs:3484`: `move_on_merge` issue update — failure now warns
    (`thegn::issues`) instead of silently leaving the issue open.
  - `media_ctl.rs:126`, `run.rs:16323`: user-invoked media transport /
    playlist-activation ops warn on failure (`thegn::media`) instead of a
    silent no-op.
  - `handlers/pr_queue.rs:226/241`: `remove_pr_entry` / `enqueue_pr` (Rewatch)
    failures now surface in the mutation's status string (mirroring the
    sibling `enqueue` handler's `PrqMsg::Failed` path) instead of claiming success.
  - `integrate.rs:220/268`: vestigial `let _ =` dropped from two
    `.output().ok()?` statements — the spawn failure was already propagated by
    `?`; the discard was pure noise.
  - `panel/gitfull.rs:247` (`let _ = focused;`) and
    `panel/sections/misc.rs:771` (`let _ = cursor_visible_pos;`): non-Result
    unused-value discards renamed (`_focused` param / `_cursor_visible_pos`
    binding) — the chunk-1 precedent — which released both files.
- **(c) flagged for reviewer: 3 sites** (below).
- **Pattern false-positives (non-Result discards) reported: 19** (below) — left
  untouched and pinned (each file keeps other matching sites).
- **Allowlist: 325 → 319 pinned** (183 → 181 `crates/thegn-host/` lines).
  Deleted exactly the two released files: `src/panel/gitfull.rs`,
  `src/panel/sections/misc.rs`. Nothing added; header untouched.

Count check: 1144 = 976 (a) + 130 (pre) + 16 (b) + 3 (c) + 19 (fp).

## Thread-spawn policy note (documented judgment call)

Thread-spawn `.ok()` sites were split by consequence, consistently across the
crate: niceties whose absence blends into "nothing happened" (sounds, desktop
toasts, push, metrics, sandbox-event listeners, perf deadline, crash-report
watcher) are (a) with "a failed spawn just disables X this session"; capabilities
that would die silently with confusing UX (forward detector, bridge fs-watch
pump, daemon control-plane TCP server) are (b) with `tracing::warn!`.

## Unsure — for the reviewer

1. `cmd/doctor.rs:2220` — `merge_guard::audit(&hooks).ok()` in
   `merge_guard_json`: on failure the doctor JSON emits `"audit": null` with no
   reason. Should the JSON carry the error (e.g. `"audit_error": …`)?
2. `panes.rs:1527` — drawer pane `spawn(..).ok()`: a failed spawn leaves the
   drawer silently unopened, no message. The same silent-`None` shape exists in
   sibling spawn helpers (`run.rs::spawn_worktree_shell_pane`), so a fix here is
   a systemic UX decision, not a site fix — left pinned + flagged.
3. `secret.rs:533` — `mcp_secret_rm` ignores `KeyringStore.del` failures while
   returning `Ok(())`: deliberate idempotent-rm semantics (absent entry), but a
   locked/failed keyring also reports success. Should non-absent failures
   return `Err`?
4. Class-note — `Db::open().ok()` before cache-only consumers
   (`agent.rs`, `hydrate_calendar.rs`, `handlers/merge_queue.rs`,
   `handlers/workspace_remove.rs`, `handlers/worktree_delete.rs`,
   `pty_drain.rs`, `run.rs:729/15862`, `cmd/share.rs`, `wizard.rs`,
   `remote_poll.rs`): annotated (a) as best-effort cache — each has a local
   fallback and the DB here is cache/resurrection state only. Flagging the
   pattern as a class in case the reviewer wants a stricter stance on
   `workspace_remove`/`worktree_delete` (stale rows after a failed open).
5. Class-note — `host_flow.rs` `run_effect` outcomes (3 sites) annotated (a) on
   the claim that effect failures re-run on the next reconcile and the drive's
   own `publish(board)`/`HostUiEvent`s carry the outcome. If that claim is
   wrong for `Checkpoint`, the annotation should be revisited.
6. Class-note — `hydrate.rs:4388-4390` watcher registrations annotated (a) with
   "a missed root just delays fs-triggered hydration until another event";
   confirm the debounce/poll path actually re-fires.

## Pattern false-positives (non-Result discards; pinned, untouched)

`actions.rs:33` (Option pane-id discard), `cmd/disk.rs:179` + `cmd/pr_queue.rs:152`
(`let _ = cfg;` signature-symmetry discards), `desktop_notify.rs:58` (`let _ = notif;`
under unsupported cfg), `detail/usage_dash.rs:92` (`let _ = (cols, rows);`),
`font.rs:431` (`let _ = family;` unused param), `handlers/pr_queue.rs:540`
(`let _ = ctx.refresh_tx;` move-to-drop), `handlers/sidebar_mouse.rs:530`
(`let _ = sb;`), `handlers/worktree_attach.rs:409` (`let _ = stream;` decoy hold),
`hibernator.rs:447` (`let _ = exec_env;`), `main.rs:872/1031`
(`clamp_to_channel` returns `Vec<Feature>`, not a Result), `hydrate.rs:1110`
(`prune_stale_worktree_groups` returns `usize`), `run.rs:13024`
(`let _ = was_center_zone;`), `search_everywhere.rs:992/1039`
(`let _ = include_hidden;`), `sidebar.rs:3247` (test fixture), `vps_reaper.rs:144`
(`let _ = env_name;`), `wire.rs:259` (`let _ = f(&mut a);` generic closure).

## Tests run (scoped per the dev-loop policy)

1. `just quick thegn-host` — clippy lib/bin: **clean** (ran before and after
   `treefmt`).
2. `cargo nextest run -p thegn-host` — **2581 passed, 8 skipped** (includes the
   help/keymap ratchet tests).
3. Ratchet:
   - Full gate: `bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates`
     → `ratchet(ignored-result): clean (319 pinned)`
   - Host-scoped spot check (the chunk-spec command with pathspec
     `crates/thegn-host`): **zero `new violation` lines**; the only `stale entry`
     lines are non-host files (thegn-svc/leaves — chunk 3's still-pinned files,
     outside this run's pathspec, an artifact of the scoped invocation). The
     host-scoped comm of hits vs `^crates/thegn-host/` allowlist lines is clean:
     new violations = ∅, released = exactly `src/panel/gitfull.rs` +
     `src/panel/sections/misc.rs`.

Reviewer spot check (done-criterion 1) — every matching site is annotated,
(b)-rewritten, or listed above; commands and counts (all measured at HEAD):

```
$ git grep -InE 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' -- crates/thegn-host \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' | wc -l
1130                     # sites still matching at HEAD (the 14 (b)-rewritten
                         # sites no longer match; 1130 = 1144 - 14)
$ ... | grep -v best-effort | while IFS=: read -r f n _; do
      sed -n "$((n-1))p" "$f" | grep -q best-effort || echo "$f:$n"; done | wc -l
48                       # sites without an on-line or directly-adjacent annotation
```

Those 48 are exactly: 15 sites whose annotation heads a multi-line statement
(2+ lines above the matched line — the design's "line above otherwise" format:
`desktop_notify.rs:31/44`, `metrics.rs:81/242`, `model_proxy_daemon.rs:253`,
`notify.rs:357`, `perf.rs:190`, `platform/unix.rs:125/199`, `profile.rs:44`,
`push_notify.rs:62`, `run.rs:6711/6731`, `sandbox_events.rs:54/67`) + the 11
(pre) prose-covered sites + 19 (fp) + 3 (c) — every one enumerated in this
report. Widening the window to the statement start (3 lines) leaves only the
22 (c)+(fp) sites, all listed.

## Unverified

- e2e (`just e2e`) not run — forbidden for this stage; the two released panel
  files' renames are behaviour-neutral (param/binding names only), and no frame
  output changed.
- `just lint` / full-workspace gates not run (pre-push territory, per policy).
- Clippy on test targets only implicitly covered via nextest's compile
  (warnings are lib/bin-gated by `just quick`); no test-target-only clippy run.
- The 15 "annotation above a multi-line statement" sites and 11 pre-prose sites
  were verified by line inspection during the audit, not by an automated check.

## Per-file table

`sites` = matching sites at base. a = annotated this chunk (comments only).
pre = already carried best-effort prose at base (left as-is). (b) = surfaced /
rewritten. c = flagged. fp = pattern false-positive (non-Result), reported.
| File | sites | a (this chunk) | pre | (b) | c | fp |
|---|---|---|---|---|---|---|
| examples/waker_spike.rs | 1 | 1 | 0 | | | |
| src/actions.rs | 21 | 14 | 4 | 2 | | 1 |
| src/agent.rs | 13 | 10 | 3 | | | |
| src/agent_configs.rs | 10 | 10 | 0 | | | |
| src/agent_home.rs | 1 | 1 | 0 | | | |
| src/agent_run.rs | 5 | 5 | 0 | | | |
| src/agent_tests.rs | 5 | 5 | 0 | | | |
| src/apps/mod.rs | 2 | 2 | 0 | | | |
| src/attention_status.rs | 1 | 0 | 1 | | | |
| src/autoscale.rs | 7 | 6 | 1 | | | |
| src/blast_radius.rs | 5 | 5 | 0 | | | |
| src/bridge_sup.rs | 4 | 3 | 0 | 1 | | |
| src/build_cache.rs | 1 | 0 | 1 | | | |
| src/ci_refresh.rs | 2 | 2 | 0 | | | |
| src/cli_help.rs | 2 | 2 | 0 | | | |
| src/clipboard.rs | 1 | 1 | 0 | | | |
| src/cmd/attach.rs | 9 | 7 | 2 | | | |
| src/cmd/ci.rs | 2 | 2 | 0 | | | |
| src/cmd/config.rs | 4 | 4 | 0 | | | |
| src/cmd/diff.rs | 1 | 1 | 0 | | | |
| src/cmd/disk.rs | 3 | 2 | 0 | | | 1 |
| src/cmd/doctor.rs | 3 | 1 | 0 | 1 | 1 | |
| src/cmd/host.rs | 4 | 4 | 0 | | | |
| src/cmd/integrate.rs | 1 | 1 | 0 | | | |
| src/cmd/merge.rs | 5 | 5 | 0 | | | |
| src/cmd/mod.rs | 2 | 2 | 0 | | | |
| src/cmd/open.rs | 1 | 0 | 1 | | | |
| src/cmd/pr.rs | 1 | 1 | 0 | | | |
| src/cmd/pr_queue.rs | 1 | 0 | 0 | | | 1 |
| src/cmd/session.rs | 2 | 1 | 1 | | | |
| src/cmd/share.rs | 5 | 5 | 0 | | | |
| src/cmd/theme.rs | 1 | 1 | 0 | | | |
| src/cmd/wt.rs | 6 | 5 | 1 | | | |
| src/config_source.rs | 2 | 0 | 2 | | | |
| src/connectivity_gate.rs | 2 | 1 | 1 | | | |
| src/daemon/client.rs | 2 | 2 | 0 | | | |
| src/daemon/inbox.rs | 2 | 0 | 2 | | | |
| src/daemon/mod.rs | 14 | 8 | 5 | 1 | | |
| src/daemon/record.rs | 3 | 3 | 0 | | | |
| src/daemon/service.rs | 11 | 10 | 1 | | | |
| src/daemon/session.rs | 18 | 16 | 2 | | | |
| src/db_task.rs | 5 | 5 | 0 | | | |
| src/desktop_notify.rs | 6 | 5 | 0 | | | 1 |
| src/detail/usage_dash.rs | 1 | 0 | 0 | | | 1 |
| src/drawer_state.rs | 8 | 5 | 3 | | | |
| src/emulator.rs | 1 | 1 | 0 | | | |
| src/env_ui.rs | 4 | 4 | 0 | | | |
| src/fff_backend.rs | 5 | 5 | 0 | | | |
| src/font.rs | 3 | 2 | 0 | | | 1 |
| src/forge_handle.rs | 1 | 0 | 1 | | | |
| src/forward.rs | 4 | 3 | 0 | 1 | | |
| src/frame_write.rs | 3 | 3 | 0 | | | |
| src/frame_writer.rs | 2 | 2 | 0 | | | |
| src/git_handle.rs | 1 | 0 | 1 | | | |
| src/git_watch.rs | 1 | 1 | 0 | | | |
| src/gitmut.rs | 7 | 7 | 0 | | | |
| src/handlers/attention.rs | 7 | 7 | 0 | | | |
| src/handlers/calendar.rs | 1 | 1 | 0 | | | |
| src/handlers/creating.rs | 1 | 1 | 0 | | | |
| src/handlers/daemon_lifecycle.rs | 1 | 1 | 0 | | | |
| src/handlers/host.rs | 4 | 3 | 1 | | | |
| src/handlers/host_heal.rs | 6 | 6 | 0 | | | |
| src/handlers/launch.rs | 1 | 1 | 0 | | | |
| src/handlers/materialize.rs | 7 | 7 | 0 | | | |
| src/handlers/merge_queue.rs | 8 | 6 | 2 | | | |
| src/handlers/mod.rs | 1 | 1 | 0 | | | |
| src/handlers/onboarding.rs | 2 | 1 | 1 | | | |
| src/handlers/overlay.rs | 1 | 1 | 0 | | | |
| src/handlers/paste_image.rs | 8 | 5 | 3 | | | |
| src/handlers/plugins.rs | 5 | 1 | 4 | | | |
| src/handlers/pr_queue.rs | 8 | 2 | 3 | 2 | | 1 |
| src/handlers/provision.rs | 7 | 7 | 0 | | | |
| src/handlers/repo_trust.rs | 2 | 2 | 0 | | | |
| src/handlers/sidebar_actions.rs | 6 | 6 | 0 | | | |
| src/handlers/sidebar_folder.rs | 2 | 2 | 0 | | | |
| src/handlers/sidebar_keys.rs | 3 | 3 | 0 | | | |
| src/handlers/sidebar_mouse.rs | 2 | 1 | 0 | | | 1 |
| src/handlers/sidebar_persist.rs | 7 | 0 | 7 | | | |
| src/handlers/sidebar_reorder.rs | 1 | 1 | 0 | | | |
| src/handlers/startup.rs | 3 | 0 | 3 | | | |
| src/handlers/status.rs | 1 | 1 | 0 | | | |
| src/handlers/switch_cache.rs | 1 | 1 | 0 | | | |
| src/handlers/task_output.rs | 2 | 1 | 1 | | | |
| src/handlers/terminal.rs | 2 | 1 | 1 | | | |
| src/handlers/tracker.rs | 9 | 9 | 0 | | | |
| src/handlers/wizard.rs | 2 | 2 | 0 | | | |
| src/handlers/workspace_remove.rs | 14 | 14 | 0 | | | |
| src/handlers/worktree_attach.rs | 4 | 0 | 3 | | | 1 |
| src/handlers/worktree_delete.rs | 1 | 1 | 0 | | | |
| src/handlers/worktree_rename.rs | 3 | 2 | 1 | | | |
| src/help/ratchet_tests.rs | 1 | 1 | 0 | | | |
| src/help/render.rs | 1 | 1 | 0 | | | |
| src/hibernator.rs | 14 | 9 | 4 | | | 1 |
| src/host_flow.rs | 34 | 33 | 1 | | | |
| src/host_provision.rs | 11 | 11 | 0 | | | |
| src/host_ui.rs | 2 | 0 | 2 | | | |
| src/hydrate.rs | 45 | 41 | 2 | 1 | | 1 |
| src/hydrate_calendar.rs | 6 | 5 | 1 | | | |
| src/hydrate_calendar_tests.rs | 2 | 2 | 0 | | | |
| src/hydrate_feed.rs | 2 | 1 | 1 | | | |
| src/hydrate_semantic.rs | 2 | 2 | 0 | | | |
| src/hydrate_tests.rs | 26 | 26 | 0 | | | |
| src/hydrate_tracker.rs | 4 | 4 | 0 | | | |
| src/integrate.rs | 18 | 12 | 4 | 2 | | |
| src/iroh_home.rs | 2 | 1 | 1 | | | |
| src/keymap.rs | 1 | 1 | 0 | | | |
| src/layout_spec.rs | 2 | 2 | 0 | | | |
| src/lifecycle.rs | 9 | 7 | 2 | | | |
| src/loc_scan.rs | 2 | 2 | 0 | | | |
| src/machine0_bridge.rs | 13 | 10 | 3 | | | |
| src/main.rs | 4 | 1 | 1 | | | 2 |
| src/managed_tool.rs | 6 | 4 | 2 | | | |
| src/mcp_proxy/upstream.rs | 2 | 1 | 1 | | | |
| src/measure/disk.rs | 7 | 5 | 2 | | | |
| src/measure/loc.rs | 4 | 4 | 0 | | | |
| src/measure/mod.rs | 2 | 2 | 0 | | | |
| src/media_art.rs | 2 | 2 | 0 | | | |
| src/media_ctl.rs | 8 | 7 | 0 | 1 | | |
| src/media_overlay.rs | 3 | 3 | 0 | | | |
| src/media_watch.rs | 1 | 1 | 0 | | | |
| src/menu.rs | 1 | 1 | 0 | | | |
| src/merge_driver.rs | 17 | 17 | 0 | | | |
| src/merge_lifecycle.rs | 34 | 34 | 0 | | | |
| src/merge_remote.rs | 5 | 4 | 1 | | | |
| src/metrics.rs | 11 | 11 | 0 | | | |
| src/model_proxy_daemon.rs | 3 | 1 | 2 | | | |
| src/monitor.rs | 2 | 2 | 0 | | | |
| src/monitor/state.rs | 1 | 1 | 0 | | | |
| src/monitor_action.rs | 3 | 3 | 0 | | | |
| src/mq_assets.rs | 10 | 8 | 2 | | | |
| src/notify.rs | 6 | 6 | 0 | | | |
| src/palette.rs | 1 | 1 | 0 | | | |
| src/pane.rs | 12 | 12 | 0 | | | |
| src/pane_pty.rs | 5 | 5 | 0 | | | |
| src/pane_writer.rs | 1 | 0 | 1 | | | |
| src/panel/gitfull.rs | 1 | 0 | 0 | 1 | | |
| src/panel/sections/misc.rs | 1 | 0 | 0 | 1 | | |
| src/panel_util.rs | 6 | 6 | 0 | | | |
| src/panes.rs | 4 | 3 | 0 | | 1 | |
| src/parity.rs | 5 | 5 | 0 | | | |
| src/perf.rs | 2 | 2 | 0 | | | |
| src/placement_flow.rs | 10 | 5 | 5 | | | |
| src/platform/proc.rs | 6 | 5 | 1 | | | |
| src/platform/unix.rs | 7 | 6 | 1 | | | |
| src/platform/windows.rs | 1 | 1 | 0 | | | |
| src/plugins.rs | 14 | 8 | 6 | | | |
| src/pr_driver.rs | 13 | 13 | 0 | | | |
| src/preview_gfx.rs | 3 | 3 | 0 | | | |
| src/preview_pane.rs | 1 | 1 | 0 | | | |
| src/probe.rs | 1 | 0 | 1 | | | |
| src/profile.rs | 2 | 2 | 0 | | | |
| src/provider_workdir.rs | 11 | 10 | 1 | | | |
| src/provision_gate.rs | 14 | 12 | 2 | | | |
| src/pty_drain.rs | 5 | 5 | 0 | | | |
| src/push_notify.rs | 1 | 1 | 0 | | | |
| src/queries.rs | 7 | 7 | 0 | | | |
| src/rasterize.rs | 1 | 0 | 1 | | | |
| src/remote_poll.rs | 9 | 9 | 0 | | | |
| src/remote_sync.rs | 4 | 3 | 1 | | | |
| src/replay_overlay.rs | 1 | 1 | 0 | | | |
| src/repo_index.rs | 1 | 1 | 0 | | | |
| src/run.rs | 126 | 120 | 3 | 2 | | 1 |
| src/run_tests.rs | 26 | 26 | 0 | | | |
| src/sandbox_events.rs | 6 | 4 | 2 | | | |
| src/search.rs | 1 | 1 | 0 | | | |
| src/search_apply.rs | 8 | 8 | 0 | | | |
| src/search_everywhere.rs | 17 | 15 | 0 | | | 2 |
| src/search_overlay.rs | 5 | 5 | 0 | | | |
| src/search_worker.rs | 10 | 10 | 0 | | | |
| src/secret.rs | 12 | 5 | 6 | | 1 | |
| src/session.rs | 7 | 7 | 0 | | | |
| src/share.rs | 5 | 5 | 0 | | | |
| src/sidebar.rs | 1 | 0 | 0 | | | 1 |
| src/sprite_bridge.rs | 2 | 2 | 0 | | | |
| src/structural_diff.rs | 3 | 2 | 1 | | | |
| src/task.rs | 26 | 26 | 0 | | | |
| src/telemetry.rs | 2 | 2 | 0 | | | |
| src/testkit/report.rs | 1 | 1 | 0 | | | |
| src/vps_reaper.rs | 3 | 1 | 1 | | | 1 |
| src/wire.rs | 4 | 3 | 0 | | | 1 |
| src/wizard.rs | 13 | 13 | 0 | | | |
| src/workspace_create.rs | 10 | 9 | 1 | | | |
| src/workspace_picker.rs | 6 | 5 | 1 | | | |
