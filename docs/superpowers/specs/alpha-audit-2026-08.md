# thegn pre-alpha audit ledger — 2026-08-14

Consolidated, adversarially-verified findings for the public-alpha audit. Each
dimension was surveyed by an independent finder; every finding below survived
1 (P2/P3) or ≥2-of-3 (P0/P1) skeptic refutation passes. Severity: **P0** crash/
data-loss/security-critical · **P1** correctness or security on a user path ·
**P2** notable defect / hardening gap · **P3** polish.

**Confirmed: 73** (9 P1, 34 P2, 30 P3).

## Fix status (2026-08-14 remediation pass)

**Fixed this pass** (commits `fix(audit):` + `fix(excise):`):

- **P1** — compositor teardown on a single pane's PTY write error (`run.rs`);
  lost persisted scrollback on the cold-workspace id-remap (`workspace_pool.rs`);
  `thegn land` exit-0-on-failure (`cmd/land.rs`); onboarding wizard's removed
  `thegn agent setup` step (`onboarding.rs`).
- **P2** — control-socket owner-only (0600) + run-dir 0700 hardening, and the
  documented-but-unimplemented "same uid" claim (`ipc.rs`, `daemon/mod.rs`,
  `config_daemon.rs`); `[serve] bind` default → loopback; `thegn serve` silent
  exit-0 when the pane daemon owns the socket; `config set` re-validate + roll
  back a config-bricking value; the CLI exit-code / silent-refusal family
  (`pr merge`, `merge`/`merge rm`, `env deprovision --all`, `wt diff --base`,
  `pair`, `ci`, `share`, `wt clean`/`rm`).
- **P3** — revtunnel + config.toml.example excision stragglers.

**Deferred → [`KNOWN_ISSUES.md`](../../../KNOWN_ISSUES.md)** (regression risk in
the ~18k-line loop, or large test-writing efforts not worth taking right before
the first release): the event-loop off-thread moves (sandbox ensure / PTY paste
/ ui_state & layout persists on the loop); daemon/serve edges (version-skew
handshake, idle-exit with the TCP listener up, disable-with-persisted-panes
duplication, resync history replay, worktree-path confinement); the merge-gate
cross-process lock; the daemon-janitor + warm-attach test gaps; test-hermeticity
flakes (shared-gate integrate tests under in-process `cargo test` — green under
the nextest gate — and `THEGN_SANDBOX=1`-sensitive sandbox tests); the remaining
CLI-shape (`--worktree` flag vs positional) and optimistic-tracker polish.

Full detail per finding follows; the inline **Status: ⏳** markers below predate
this summary and are superseded by it.

## P1

### [cli-api] `thegn land` exits 0 when the land fails (conflict / gate red / unreachable)

- **Where:** `crates/thegn-host/src/cmd/land.rs:67` · effort: small
- **Evidence:** run() matches every AttemptOutcome and returns Ok(()): `AttemptOutcome::Conflict { paths } => { outln!("✗ {branch} conflicts with {target}: {}", paths.join(", ")); }` … `AttemptOutcome::GateFailed { .. } => { outln!("✗ {branch} breaks the build (gate red); not landed."); }` … ends `Ok(())` (line 80). Identical pattern in cmd/merge.rs land() lines 343-355 (Conflict/GateFailed/Unreachable arms all fall through to `Ok(())`).
- **Impact:** CLAUDE.md blesses `thegn land` as THE way for sandboxed agents/scripts to merge to main. A script `thegn land && git branch -D …` or an agent chaining on exit code sees success while nothing landed — the failure states print a line but the process exits 0, so automation proceeds on a branch that is still unmerged (potential lost work / skipped gates). Violates the repo's own docs/cli.md exit-code contract (1 = error).
- **Fix:** Return a non-zero exit for Conflict/GateFailed/Unreachable (e.g. `std::process::exit(cmd::EXIT_ERROR)` or a typed error; Unreachable could use EXIT_RETRYABLE). Apply the same to `thegn merge land` and to `merge drain` when everything deferred (or add a documented `--exit-status` contract).
- **Status:** ⏳

### [cli-api] `env deprovision --all` destroys every sandbox on the provider account with no confirmation

- **Where:** `crates/thegn-host/src/cmd/env.rs:302` · effort: small
- **Evidence:** `if all { let ids = crate::agent::block_on_provider(|| async { provider.list().await })…; for id in &ids { match … provider.destroy(id) … }` — doc comment says "With `--all`, list every sandbox the provider can see and destroy each". No `confirm()` call and no `--force` gate anywhere in `deprovision`, unlike `wt rm` (wt.rs:290 prompts) and `pr merge` (pr.rs:245 prompts).
- **Impact:** One command irreversibly destroys ALL provider sandboxes visible to the token — including sandboxes belonging to other worktrees, other thegn instances, or non-thegn resources on a shared Daytona/Sprites/Fly/VPS account. Remote data (uncommitted work inside in_env sandboxes) is lost. The most destructive verb in the CLI is the only destructive verb without a prompt.
- **Fix:** Require `super::confirm(&format!("destroy {} sandbox(es) on {}?", ids.len(), provider…))` unless a new `--force` flag is passed (mirror `wt rm`'s `force: bool`).
- **Status:** ⏳

### [daemon-separation] `thegn serve` silently exits 0 without a TCP listener whenever the compositor's pane daemon already owns the socket — which is the default configuration

- **Where:** `crates/thegn-host/src/daemon/mod.rs:135` · effort: small
- **Evidence:** mod.rs:135-138: `thegn_svc::ipc::BindOutcome::AlreadyRunning => { tracing::info!(..."daemon already running on {}"...); return Ok(()); }` — this return fires BEFORE the `if let Some(opts) = serve` block (line 230) that binds TCP, registers `tcp_addr`, and prints the pairing URL. `[daemon] enabled = true` is the default (config_daemon.rs:33), so a compositor keeps a daemon warm on the exact same socket path (`ensure_daemon` uses `super::socket_path(dcfg)`), and the only diagnostic is a tracing line that is invisible unless THEGN_LOG is set.
- **Impact:** The most common real-world invocation of `thegn serve` (user has run the TUI before) produces no output, exit code 0, and no listening control plane. Remote pairing/thin clients cannot connect and the user has no error to act on. User-invoked primary path swallowing its failure.
- **Fix:** In serve mode, treat AlreadyRunning as an error printed to stderr with the remedy (e.g. "a pane daemon already owns <sock>; stop it or run `thegn serve` from that daemon"), exit non-zero — or better, connect to the live daemon and instruct it to open the TCP listener (RPC) so serve works either way.
- **Status:** ⏳

### [event-loop] A single pane's PTY write error tears down the entire compositor (`?` on write_input in the key/paste dispatch)

- **Where:** `crates/thegn-host/src/run.rs:17628` · effort: small
- **Evidence:** run.rs:17628 `p.write_input(&batched)?;` (single-pane key forward), run.rs:13355 `p.write_input(&bytes)?;` (Ctrl+g keybind-lock forward), run.rs:17765-17769 `p.write_input(b"\x1b[200~")?; p.write_input(s.as_bytes())?; ...` (paste path) — all inside the event loop of `pub async fn main(cli: crate::Cli) -> Result<()>` (run.rs:394), so the `?` propagates out of the loop and exits the whole app. The write is a real fallible syscall: pane.rs:662 `writer.write_all(bytes).context("pty write")?`, and pane*pty.rs:99 `drop(pair.slave); // Drop the slave so the master sees EOF when the child exits` means writes to the master return EIO once the child exits. The Exit event rides the pane channel and is only processed at the NEXT drain, so a keystroke dispatched in the same wake the child died (e.g. typing right as a shell exits/crashes) hits the dead master and kills every worktree/pane/session. The sibling broadcast path three lines up (run.rs:17624) already uses `let * = p.write_input(&batched);`, showing the propagation is unintentional asymmetry, not policy.
- **Impact:** User-invoked crash: typing or pasting into a pane whose child just exited (a routine race — `exit` + trailing keys, a crashing TUI, an ssh drop) can terminate the entire compositor with `Error: pty write`, dropping every open worktree/terminal in the session.
- **Fix:** Never propagate per-pane write errors out of the loop. Replace the `?` at run.rs:13355, 17628, and 17765-17769 with the sanctioned ignore (`let _ =`, matching 17624) or, better, on write error mark the pane dead and surface `model.status` — the Exit event arriving on the next drain already handles cleanup.
- **Status:** ⏳

### [event-loop] Sandbox ensure / provider network calls inside `launch_spec` run ON the event loop via the crash-respawn and split-pane paths

- **Where:** `crates/thegn-host/src/pty_drain.rs:812` · effort: medium
- **Evidence:** pty_drain.rs:812 `match spawn_worktree_shell_pane(` — called synchronously from `handle_exit` (the on-loop PTY drain) to respawn a worktree tab's sole shell after any child exit. run.rs:15491 `None => spawn_worktree_shell_pane(` does the same on the SplitDown/SplitRight action, and drawer_state.rs:290 on the drawer shell fallback. `spawn_worktree_shell_pane` (run.rs:4721) calls `crate::agent::launch_spec(cfg, &wt, None, "shell")?`, which the codebase itself documents as loop-unsafe: run.rs:7096-7098 "request launch specs off-thread (the sandbox ensure inside `launch_spec` can block on podman for seconds to minutes)", agent.rs ~2790 "idempotent, off the event loop — launch_spec is blocking", and agent.rs:1400 `block_on_provider(|| async { provider.ensure_exists(&id).await })` — a provider NETWORK round-trip. The materialize and prewarm paths route the identical spec resolution through `spawn_blocking` + `spec_tx` (run.rs:6925, 7115); these three call sites bypass that machinery.
- **Impact:** The UI freezes (no input, no render, no waker service) for the duration of a podman/container ensure or a provider ensure_exists network call — the code's own comment says seconds to minutes — whenever a sandboxed worktree's shell exits and respawns, a user splits a pane in a sandboxed worktree, or the drawer falls back to a shell. During a crash-storm (up to 3 fast respawns) this repeats back-to-back. Directly violates the stated hard invariant (never put git/DB/subprocess on the loop).
- **Fix:** Route the respawn and split-pane shell spawn through the existing two-phase machinery: resolve the spec on `spawn_blocking` (as `handlers::materialize` / the prewarm block at run.rs:6925 already do, sending on `spec_tx` + waker) and attach the pane when the spec lands, keeping only openpty+exec on the loop.
- **Status:** ⏳

### [races] Reused merge-gate worktree has no cross-process lock — concurrent land/drain can gate the wrong tree and land unverified commits

- **Where:** `crates/thegn-host/src/integrate.rs:420` · effort: small
- **Evidence:** gate_tip() reuse path: `util::git_ok(&wt, &["checkout", "--detach", "--force", oid])` into the stable per-repo dir from gate_base() (`util::xdg_state_home().join("thegn/gate").join(format!("{name}-{key:016x}"))`, lines 344-356), then runs `sh -c gate_command` in that same dir. The only serialization is the comment at line 385: "Reuse depends on drains being serialized (queue design)" — but the sole guard is the loop-local `fold_inflight: bool` in handlers/merge_queue.rs:181-202, which serializes only within ONE compositor process. attempt_land is also invoked by the CLI `thegn land` (cmd/land.rs:44) and `thegn integrate`/`merge drain` — separate processes with no lock (grep for flock/lockfile in integrate.rs/merge_driver.rs/cmd/land.rs/merge_guard.rs: none). Two concurrent lands on the same repo: A checks out oidA and starts its gate; B `checkout --force oidB` mid-gate; A's gate verdict now applies to B's tree.
- **Impact:** The gate is the ONLY check between fold and CAS-advancing the target branch. A false-green lands an untested/broken fold commit on main (data integrity of the default branch); a false-red defers a good branch and can mis-bisect an 'offender'. Concurrent `thegn land` from multiple agent worktrees is the normal workflow in this repo, so the race is realistic, and it is the credible production-race family behind attempt_land instability under parallel load (the unit tests themselves use per-test repos, so the observed test panic is more consistent with git-subprocess failure surfacing through `.unwrap()` — but production gate corruption is real and silent).
- **Fix:** Take an exclusive advisory file lock (flock) on a sidecar file under gate_base(repo_root) for the whole prepare-checkout+gate_command span (blocking with a timeout, or fall back to a throwaway /tmp worktree when contended). Alternatively make the reused worktree per-(repo,pid) with a shared CARGO_TARGET_DIR (cargo's own target-dir locking then serializes builds).
- **Status:** ⏳

### [session-persist] remap_cold_workspace_ids drops pane_scrollback: persisted scrollback is never restored and is destroyed on the next persist

- **Where:** `crates/thegn-host/src/workspace_pool.rs:146` · effort: trivial
- **Evidence:** Lines 146-157 remap exactly three maps and stop: `tab.pane_cwds = std::mem::take(&mut tab.pane_cwds)…`, `tab.pane_cmds = …`, `tab.pane_sessions = …` — `tab.pane_scrollback` is never remapped, unlike the (dead-code, `#[allow(dead_code)]`) `Session::remap_pane_ids` in session.rs:588-593 which does remap it. Every cold resurrect goes through this function (run.rs:5955 at startup, run.rs:1858 on cold workspace switch), rewriting the tree's leaf ids onto a reserved range. The restore consumers then look up by the NEW id: panes.rs:847 `let Some(text) = tab.pane_scrollback.get(old)` (host-pane repaint) and panes.rs:738 `p.set_fallback_restore(tab.pane_scrollback.get(old).cloned(), …)` (daemon-reattach fallback restore) — `old` is the post-remap id, the map keys are pre-remap ids, so both get `None` whenever the remap is non-identity (any session where the previous run's spawn order didn't produce ids 1..n contiguous in tab-iteration order — e.g. a drawer/pin pane consumed an id, a split was closed, or a branch worktree materialized before home). Worse, `Tab::to_row` (session.rs:181-185) prunes scrollback entries whose keys aren't in the current tree, so the first `persist_session_layout` after launch permanently deletes the orphaned old-keyed entries.
- **Impact:** The headline behavior of the just-shipped persistence feature — 'a resurrected pane shows its recent history rather than a blank screen' (session.rs:99-103) and the dead-daemon-session fallback repaint (daemon_lifecycle.rs:92-111, 'Persistent session expired' with scrollback) — silently shows a blank pane in most real sessions, and the captured scrollback is then dropped from the DB. Unit tests pass because their simple sessions produce identity remaps.
- **Fix:** Add the same remap block for `pane_scrollback` in `remap_cold_workspace_ids` (mirroring `pane_sessions`), or delete the divergent copy and call `Session::remap_pane_ids` (which already handles all four maps) with the per-tab reserved-base mapping. Add a run_tests case asserting scrollback keys follow the leaf through the remap (the existing tests at run_tests.rs:1934/1978 only cover cwds/focus).
- **Status:** ⏳

### [test-gaps] Default pane transport (daemon WS warm-attach pipeline) has zero automated coverage

- **Where:** `crates/thegn-svc/src/control/client.rs:339` · effort: medium
- **Evidence:** `async fn pump_attach_ws(` — control/client.rs has 0 tests (457 LOC); the server side `.route("/v1/sessions/{s}/attach", get(attach_ws))` (control/http.rs:71) is covered only by 4 trivial `expiry_ms` tests; the compositor bridge `fn adapt(session_id: String, stream: AttachStream) -> ExecSession` (thegn-host/src/daemon/client.rs:165) has 0 tests. Meanwhile `[daemon]` is ON by default: "enabled: true … new local center panes route through the daemon" (thegn-core/src/config_daemon.rs:33). test/smoke.sh opens sessions via curl + `session snapshot` (HTTP only, never the WS attach stream); pty-smoke.sh exits at first frame (`THEGN_BENCH_FIRST_FRAME_EXIT=1`) before any pane content is asserted.
- **Impact:** A regression anywhere in the WS attach path (frame encode/decode, snapshot-before-delta ordering, stdin routing, SessionExit→Exit mapping in adapt — note adapt maps `code: None` to `Exit(0)`) ships undetected and manifests as EVERY new pane blank/dead or input-dead for every alpha user, since this is the default route. Daemon actor/service unit tests (10 tests) stop at the mailbox; nothing exercises client↔HTTP/WS↔daemon end to end.
- **Fix:** One in-process integration test: bind `control::http::router` + a real `DaemonService` on a temp unix socket (the daemon's own run() wiring, minus serve mode), then drive `ControlClient::open` + `attach`: assert (a) first frame is a PaneSnapshot and the next delta is snapshot.seq+1, (b) `AttachControl::Input` bytes echo back through a `cat` session, (c) killing the session yields the Exit frame through `adapt`. This single test locks the whole default pane transport.
- **Status:** ⏳

### [ux-polish] First-run onboarding wizard's 'coding agent' step spawns the removed `thegn agent setup` subcommand

- **Where:** `crates/thegn-host/src/handlers/onboarding.rs:318` · effort: medium
- **Evidence:** handlers/onboarding.rs:316-327: `if effects.agent_setup { spawn_wait_tab("thegn agent setup", false, ...) }`. The step's copy in onboarding.rs:1357 offers "run `thegn agent setup` now" and 1363-1364 say "installs + configures the managed coding agent (pi)." / "also available later: `thegn agent setup`." — but main.rs's `enum Command` (lines 216-530) has no `agent` variant (verified: only Pr/Issue/.../Setup/.../SpriteExec), and managed-pi was excised. The pane runs `$SHELL -lc "thegn agent setup"` (panes.rs:233 `tool_drawer_argv`), so clap prints "unrecognized subcommand 'agent'" and the tab exits; `on_pane_exit` silently resumes the wizard with no explanation (is_login=false branch, handlers/onboarding.rs:369).
- **Impact:** The very first thing a new user sees (the auto-opened setup wizard, step 8/9) advertises and launches a feature that was just removed. Opting in flashes an error tab and drops the user back into the wizard with no feedback — a broken promise on the flagship first-run path of the public alpha. The test `agent_toggle_requests_setup_spawn` (onboarding.rs:1783) locks the dead behavior in.
- **Fix:** Remove `Step::Agent` from STEPS, `Field::AgentAction`, `Effects.agent_setup`, and the `spawn_wait_tab("thegn agent setup", ...)` arm (or repoint the step at documenting `[[agents]]` config with no spawn). Update the module doc (onboarding.rs:5-6 "and a coding agent") and delete/replace the `agent_toggle_requests_setup_spawn` test.
- **Status:** ⏳

## P2

### [cli-api] Compositor-path errors are swallowed: exit 1 with no message printed

- **Where:** `crates/thegn-host/src/main.rs:755` · effort: trivial
- **Evidence:** `let result = rt.block_on(run::main(cli)); … let code: i32 = match &result { Ok(()) => cmd::EXIT_OK, Err(_) => cmd::EXIT_ERROR, }; std::process::exit(code);` — the `Err` value is never formatted/printed. run::main returns early errors like `.context("term capabilities")?` / `.context("open terminal")?` (run.rs:450-453), and during the session stderr was redirected to the logfile (run.rs `redirect_stderr_to_logfile`), so nothing else surfaces it.
- **Impact:** The most common misuse — launching `thegn` where /dev/tty can't be opened (piped, cron, minimal container, broken TERM) — produces exit code 1 and zero output. Users have no idea what went wrong; for a public alpha this is the first impression failure mode.
- **Fix:** Before `std::process::exit(code)`, on Err print the error chain to stderr (the stderr guard has been dropped by then): `if let Err(e) = &result { thegn_core::msg::error(&format!("{e:#}")); }`.
- **Status:** ⏳

### [cli-api] `pr merge` has no --yes flag; non-interactive invocation silently cancels with exit 0

- **Where:** `crates/thegn-host/src/cmd/pr.rs:245` · effort: small
- **Evidence:** `if !confirm(&format!("Merge this PR ({method:?})?")) { msg::info("cancelled"); return Ok(()); }` — `Action::Merge` (pr.rs:56-65) has `--method/--delete-branch/--auto` but no `--yes`/`--force`. `cmd::confirm` (mod.rs:110-128) returns false on stdin EOF, so a scripted `thegn pr merge` prints "cancelled" and exits 0.
- **Impact:** In any non-TTY context (scripts, CI, agents) the merge silently no-ops AND reports success via exit code — automation believes the PR was merged. There is no way to use `pr merge` non-interactively at all.
- **Fix:** Add a `--yes` flag that skips the prompt (consistent with `wt rm --force` / `wt clean --force`), and exit non-zero (or at least a distinct code) when the confirmation is declined/unavailable.
- **Status:** ⏳

### [cli-api] Merge-queue verbs exit 0 when refused: queue disabled and remote-target guard paths

- **Where:** `crates/thegn-host/src/cmd/merge.rs:62` · effort: small
- **Evidence:** `if !cfg.merge_queue.enabled { outln!("Merge queue disabled. Set `[merge_queue]` `enabled = true` …"); return Ok(()); }` gates ALL of `merge add/rm/clear/drain/land` (and integrate.rs:20-25 identically). Likewise the remote-target guard: land.rs:50-56 `if … remote_target_guard(…) { outln!("{msg}"); return Ok(()); }` and integrate.rs:33-39, merge.rs:234-239.
- **Impact:** `thegn merge add` / `thegn land` / `thegn integrate` in scripts return success while doing nothing (queue disabled, or target on another host). A CI job that enqueues branches or lands them cannot detect the refusal; work silently never lands.
- **Fix:** Exit non-zero (EXIT_ERROR, message on stderr) when a user-invoked mutation is refused; keep exit 0 only for genuinely-empty cases like `merge list` / "Nothing to drain."
- **Status:** ⏳

### [cli-api] `wt diff --base <typo>` silently diffs against HEAD; git errors swallowed entirely

- **Where:** `crates/thegn-host/src/cmd/diff.rs:27` · effort: small
- **Evidence:** `let target = loc.git_out(&["merge-base", &base, "HEAD"]).unwrap_or_else(|| "HEAD".to_string());` — a nonexistent `--base` ref makes merge-base fail and the fallback quietly becomes HEAD (uncommitted-only diff). Then `if let Ok(output) = loc.git_command(git_args).output() { … }` (line 33) captures and discards stderr and ignores `output.status`, so any git failure prints nothing and exits 0.
- **Impact:** A user who typos `--base` gets a plausible-looking but wrong diff (only uncommitted changes) with exit 0 and no warning — a correctness bug on a user-invoked read path that can mislead reviews. Genuine git failures produce empty output with success.
- **Fix:** Validate the base ref (`rev-parse --verify`) and error with EXIT_NOT_FOUND on a bad `--base`; check `output.status` in `emit_highlighted` and surface git stderr on failure.
- **Status:** ⏳

### [cli-api] `env set/show/up/down` use a private worktree resolver that ignores $THEGN_WORKTREE and the git toplevel

- **Where:** `crates/thegn-host/src/cmd/env.rs:808` · effort: trivial
- **Evidence:** `fn resolve_worktree(worktree: Option<String>) -> String { worktree.or_else(|| std::env::current_dir()…)…}` — unlike the shared `cmd::resolve_worktree` (mod.rs:92-106: arg → $THEGN_WORKTREE → git toplevel → cwd) used by wt/pr/merge/land. `set()` then persists `db.set_worktree_env(&wt, name)` keyed by the raw cwd.
- **Impact:** Running `thegn env set foo` (or `env show`) from a subdirectory of a worktree — or inside a sandboxed pane where $THEGN_WORKTREE is the canonical path — records the selection under the wrong key, so the compositor (which resolves envs by worktree root) never sees it: the command silently no-ops. `env show` likewise misreports the resolved env from a subdir.
- **Fix:** Delete the local resolver and use `super::resolve_worktree(worktree).to_string_lossy()` so env verbs share the arg→env→toplevel→cwd chain.
- **Status:** ⏳

### [cli-api] `pair revoke`/`pair approve` report success for nonexistent or already-settled ids

- **Where:** `crates/thegn-host/src/cmd/pair.rs:157` · effort: small
- **Evidence:** `PairAction::Revoke { id } => { db.revoke_pairing(&id, now_ms())?; outln!("revoked {id}"); }` — `revoke_pairing` (thegn-core/src/db_control.rs:314-320) runs `UPDATE pairings SET revoked_at = ?2 WHERE pairing_id = ?1 AND revoked_at IS NULL` and ignores the affected-row count; approve_pairing (db_control.rs:305-312) is identical.
- **Impact:** Security-relevant misreport: an operator who typos a pairing id when cutting off a thin client's access sees "revoked <id>" with exit 0 while the real credential stays live. Same for approving — "approved" prints even when nothing matched.
- **Fix:** Have revoke/approve return the affected-row count (rusqlite `execute` already returns it); in cmd/pair.rs bail with a NotFound error (exit 3) when 0 rows changed.
- **Status:** ⏳

### [cli-api] `ci` mutations no-op with exit 0 when no provider resolves; `ci runs --json` emits non-JSON on error with exit 0

- **Where:** `crates/thegn-host/src/cmd/ci.rs:127` · effort: small
- **Evidence:** `None => { outln!("no CI provider for this worktree (set [ci] provider, or check the remote)"); None }` — rerun/trigger/cancel (lines 274-339) all start `let Some((loc, client)) = client(cfg, worktree) else { return Ok(()); };`, so a mutation with no provider exits 0. And in `runs()` the error arm `Err(e) => outln!("ci: {e}")` (line 199) runs even with `--json`, printing a bare non-JSON line to stdout and exiting 0.
- **Impact:** Scripts triggering/cancelling CI can't detect the no-provider misconfiguration (silent no-op, success code). `ci runs --json` consumers get a parse failure or, worse, treat the run as "no data" — violating the documented one-JSON-document `--json` contract (docs/cli.md:39-47).
- **Fix:** For rerun/trigger/cancel, make the no-provider path an error (msg::die/anyhow::bail). In `runs()`/`view()`/`log()` with `--json`, emit a JSON error object (mirroring session.rs's `{"error":"no_daemon"}`) and/or exit non-zero.
- **Status:** ⏳

### [cli-api] `share start` flag/config misuse exits 0 (invalid --reach, provider disabled, public disallowed)

- **Where:** `crates/thegn-host/src/cmd/share.rs:68` · effort: small
- **Evidence:** `Err(e) => { outln!("share: {e}"); return Ok(()); }` for a bad `--reach` value; `else { outln!("share: disabled (set [share] provider, or that reach)"); return Ok(()); }` (line 75-77); `outln!("share: public sharing is disabled …"); return Ok(())` (lines 79-81).
- **Impact:** A user-invoked action that did not do what was asked (no tunnel was started) reports success. An invalid enum value for `--reach` in particular is a flag-validation error — every other clap-validated flag exits 2 on bad values, so this is inconsistent and script-hostile.
- **Fix:** Use `#[arg(long, value_enum)]` for `--reach` so clap rejects bad values, and `anyhow::bail!` for the disabled/allow_public refusals (the `on_error = warn` knob already exists for the launch-failure case and can stay).
- **Status:** ⏳

### [cli-api] docs/cli.md — the "stable CLI contract" served to agents via `thegn://doc/cli` — is stale and overclaims

- **Where:** `docs/cli.md:41` · effort: small
- **Evidence:** "Every list-shaped read surface accepts `--json`" — false: `zone list` has no `--json` (cmd/zone.rs:16 `List,` bare), nor do `mcp list`/`theme list`; the enumeration also omits `merge list`, `session list`, `pair list` which do. The Grammar table (lines 11-17) is missing whole shipped namespaces: `land`, `merge`, `zone`, `kaneo`, `placement`, `serve`, `session`, `attach`, `pair`, `setup` (compare cli_help.rs GROUPS lines 15-34). This file is embedded and served as the "stable CLI grammar" by `thegn mcp serve` (cmd/mcp.rs:28 `const CLI_DOC: &str = include_str!("../../../../docs/cli.md")`).
- **Impact:** The self-described stable automation contract shipped to coding agents through thegn's own MCP endpoint misdescribes the grammar and the --json surface, so agents/scripts written against it break (e.g. `thegn zone list --json` → clap error exit 2).
- **Fix:** Regenerate the grammar table from cli_help::GROUPS (or add a drift unit test like the one for GROUPS), correct the --json enumeration, and add `--json` to `zone list` for consistency.
- **Status:** ⏳

### [config-db] Startup orphan GC removes ALL thegn containers when the worktrees query errors

- **Where:** `crates/thegn-host/src/run.rs:717` · effort: trivial
- **Evidence:** let db_worktrees: Vec<String> = db.worktrees().unwrap_or_default()... let removed = thegn_core::sandbox::run_gc(&db_worktrees); — sandbox.rs:2146 run_gc `rm -f`s every `thegn-*` container not matching an active-worktree slug, and identify_orphans (sandbox.rs:2120) claims "Reaping is fail-closed". `unwrap_or_default()` collapses a Db read error (lazy corruption detection, a locked statement past the 5s busy_timeout, or a newer-schema branch DB whose `worktrees` table shape makes the SELECT in db_workspace.rs:423 fail) into an EMPTY active list, so every container — including live sandboxes of a concurrently running instance sharing the DB — is force-removed at startup.
- **Impact:** Data loss on a transient/foreign-schema DB error: all running sandbox containers (agent/shell sessions inside them) are killed. This is exactly the "newer schema row meets this build" hazard — the fail-closed contract is defeated by the error path.
- **Fix:** Match on `db.worktrees()`: on `Err`, log and skip GC entirely (return) instead of defaulting to an empty list. Optionally also skip GC when `db.schema_mismatch().is_some()`.
- **Status:** ⏳

### [config-db] Session resurrect swallows DB errors into an empty session; next clear-then-insert persist wipes the stored layout

- **Where:** `crates/thegn-host/src/hydrate.rs:499` · effort: small
- **Evidence:** let mut session = Session::resurrect_with_cfg(&db, &session_name, cfg).unwrap_or_default(); — resurrect_with_cfg (session.rs:312-319) propagates `group_tabs_for_session`/`groups_for_session` errors (locked/corrupt/newer-schema DB). `unwrap_or_default()` yields an empty session, load_or_seed_session then seeds a fresh single home tab, and the next Session::persist (session.rs:473-475: `db.transaction(|db| { db.clear_session_layout(session)?; ... })`) deletes every previously stored tab_groups/group_tabs row for the session.
- **Impact:** One transient DB read error at launch permanently discards the user's whole persisted layout: pane trees, per-pane cwds/cmds, provider exec sessions, and scrollback snapshots (v14/v15/v23/v29 columns).
- **Fix:** Distinguish Err from Ok(empty): on resurrect error, either fail into the no-DB ephemeral-session branch (which never persists over the old rows) or set a flag that suppresses clear_session_layout until one successful resurrect has occurred.
- **Status:** ⏳

### [config-db] validate_str covers ~12 of ~50 config_enum keys — `thegn config validate` exits 0 on invalid enum values

- **Where:** `crates/thegn-core/src/config.rs:4494` · effort: medium
- **Evidence:** validate_str spot-checks only picker, worktree_mode, name_scheme, sandbox.{backend,network,profile,on_missing}, sandbox.remote.{transport,mode}, log.{level,format}, and pins[].location (config.rs:4494-4547). The repo has 50+ `config_enum!` declarations (config_theme.rs, config_ui.rs, config_ci.rs, config_placement.rs, config_remote.rs, config_vpn.rs, config_env_tables.rs, toolchain.rs, plus ~15 more in config.rs: WarmDirenv, DotfileMode, ShellStrategy, PlacementMode, PinCorner, PinScope, ConflictHandoff, OnLanded, media backend, ...). Their deserialization is deliberately infallible (macro at config.rs:115-123 warns-and-defaults), so the wholesale `toml::from_str::<Config>` check at 4476 never flags them. `thegn config set`'s sibling CLI doc claims "Strictly validate the config file; non-zero exit on any problem" (crates/thegn-host/src/cmd/config.rs:33).
- **Impact:** e.g. `on_landed = "nuke"`, `theme color = "truecolour"`, `merge_queue conflict_handoff = "agents"` pass `config validate` with exit 0 (only a stderr warn during deser), then silently run on defaults — for OnLanded the enum default (Off) even differs from the documented struct default (remove), so validated configs behave differently than written.
- **Fix:** Make validation generic instead of hand-listed: have `config_enum!` register each (dotted key path, from_str_validated) pair, or walk the TOML against the schemars schema enum values in validate_str, so every enum key is strict-checked. At minimum add the merge_queue/theme/placement/media enums.
- **Status:** ⏳

### [config-db] `thegn config set` never re-validates — a mistyped value for a typed field makes the entire config revert to defaults at load

- **Where:** `crates/thegn-host/src/cmd/config.rs:55` · effort: small
- **Evidence:** Action::Set { key, value } => { thegn*core::config_write::set_key(&path, &key, &value)?; outln!("set {key} = {value:?} ...") } — no validate after the write. config_write.rs:143-149 documents the exact failure: "writing a quoted string into [a typed field] makes `toml::from_str::<Config>` hard-error, and `load_layered` then discards the \_entire* file and reverts to defaults". typed_value only rescues well-formed `true`/`false`/ints/floats; `thegn config set pr.ttl_secs 2m` (or any typo into a bool/u64 field) still writes `ttl_secs = "2m"`, and try_load_layered (config.rs:3874) then fails wholesale → load_layered (config.rs:3917-3931) warns to stderr and runs on pure defaults.
- **Impact:** One `config set` typo silently disables the user's whole configuration (sandbox hardening, keybinds, envs) on next launch — the warn is easily lost before the alt screen opens. The file itself is intact but inert.
- **Fix:** After set_key, run config::validate_str + the wholesale Config parse on the resulting body; on failure, revert the write (the old body is in hand) and print the error, or at least print a loud "config will be IGNORED on next launch" diagnostic with non-zero exit.
- **Status:** ⏳

### [config-db] Sidebar/panel ui_state persists still open a fresh Db inline on the event loop (5s busy_timeout ceiling per statement)

- **Where:** `crates/thegn-host/src/handlers/sidebar_persist.rs:124` · effort: medium
- **Evidence:** pub(crate) fn persist(&self, key: &str, value: &str) { if let Ok(db) = thegn_core::db::Db::open() { ... db.set_ui_state(...) } } (and unpersist at :135, panel_util.rs:71-93) — called directly from key handlers (handlers/sidebar_keys.rs:577,729,732,...). Db::open() re-runs the WAL pragmas, the full CREATE batch, additive_schema's ~30 ALTER attempts, AND a prune_notifications DELETE (db.rs:652) — i.e. multiple write-lock acquisitions with busy_timeout(5000) (db.rs:176) — synchronously on the compositor loop. crates/thegn-host/src/db_task.rs:4-10 exists precisely for this ("many loop-side best-effort cache persists — yank registers, `ui_state` toggles, pin state — used to call Db::open() inline... Routing those writes through this thread keeps the loop non-blocking") but these sites were never routed.
- **Impact:** With a concurrent writer holding the lock (a CLI `thegn merge add`/`thegn open`, or a second instance sharing the DB — a workflow CLAUDE.md explicitly supports), a mere collapse/pin toggle can stall the UI for multiple seconds; violates the event-loop no-blocking-I/O invariant on a user-invoked path.
- **Fix:** Route SidebarState::persist/unpersist and panel_util's writes through db_task::persist (the queued writer) — this is the documented remaining DB-writer sweep debt.
- **Status:** ⏳

### [daemon-separation] Unix-socket 'local admin' has no peer-credential check and no socket permission hardening — any process that can connect gets full admin (arbitrary command execution)

- **Where:** `crates/thegn-svc/src/ipc.rs:218` · effort: small
- **Evidence:** ipc.rs:218 `tokio::net::UnixListener::bind(sock)` — no `set_permissions` on the socket or its parent; daemon/mod.rs:123 `std::fs::create_dir_all(parent).ok();` (default mode); http.rs:129-130 `let ctx = if state.local_admin { AuthCtx::local_admin() }` grants admin to ANY connector with zero verification. Meanwhile config_daemon.rs:68-69 documents: "Unix-socket peers (same uid, via peer credentials) get implicit admin" — no peer-credential code exists anywhere (grep for SO_PEERCRED/peer_cred: only hits are unrelated). `thegn_core::fsperm` (restrict_file 0600 / restrict_dir 0700) exists but is unused here.
- **Impact:** Auth is purely the filesystem mode the umask happened to produce. The XDG_RUNTIME_DIR path is safe (0700 parent), but the documented fallback `$XDG_STATE_HOME/thegn/run/daemon.sock` (no XDG_RUNTIME_DIR: ssh without logind, cron, containers) is created with default perms; with umask 002 on a shared-group system or 000, another local user connects and holds admin scope — `open` runs arbitrary argv as the daemon's user. Docs promise a control that is not implemented.
- **Fix:** After bind: `fsperm::restrict_dir` (0700) on the socket's parent and chmod 0600 the socket file; optionally verify `UnixStream::peer_cred()` uid == our uid before granting local_admin (and fix the config doc if peercred is not added).
- **Status:** ⏳

### [daemon-separation] `[serve] bind` defaults to plaintext 0.0.0.0:5380 — bearer tokens and full PTY I/O in cleartext on all interfaces by default

- **Where:** `crates/thegn-core/src/config_daemon.rs:76` · effort: trivial
- **Evidence:** config_daemon.rs:76 `bind: "0.0.0.0:5380".into()`. The daemon's own comment (daemon/mod.rs:227-229) says: "v1 is plaintext: bind to a trusted interface (tailscale/wireguard) or reach it over `ssh -L`" — but the default contradicts that guidance. Every request carries the bearer token in cleartext (client.rs:432 `req.header(AUTHORIZATION, format!("Bearer {t}"))`), and attach streams carry raw keystrokes/screen bytes.
- **Impact:** A public-alpha user who runs `thegn serve` without reading docs exposes a token-gated but sniffable remote-control plane (session open = command execution if a write/admin token leaks) on every interface, including untrusted LANs.
- **Fix:** Default `bind` to `127.0.0.1:5380`; require an explicit `--bind 0.0.0.0:...` (and print a plaintext warning when binding a non-loopback address).
- **Status:** ⏳

### [daemon-separation] `thegn serve` self-terminates after idle_exit_secs (default 30 min) with zero sessions, killing the TCP control plane under paired clients

- **Where:** `crates/thegn-host/src/daemon/mod.rs:208` · effort: small
- **Evidence:** mod.rs:208-214 spawns `idle_exit_loop` unconditionally, including serve mode; the busy check is sessions-only: mod.rs:409 `let busy = !svc.sessions.lock().await.is_empty();`. Connected event-feed/HTTP/gRPC clients (pairing, git status, merge verbs — none of which require a session) do not count. The config doc claims otherwise: config_daemon.rs:22-23 "Exit after this long with no sessions and no clients".
- **Impact:** An operator starts `thegn serve`, pairs a phone, opens no PTY session — 30 minutes later the foreground server exits (graceful shutdown tears down the TCP listener too) and remote clients get connection refused. Doc/behavior mismatch also mis-sets expectations for the plain daemon.
- **Fix:** Disable (or greatly lengthen) idle-exit while a `ServeOpts` TCP listener is active, or count live HTTP/WS/gRPC connections as busy; align the config doc with the implemented check.
- **Status:** ⏳

### [daemon-separation] Disabling the daemon ([daemon] enabled=false / THEGN_NO_DAEMON) with persisted daemon panes silently duplicates every pane's process and leaks the old sessions forever

- **Where:** `crates/thegn-host/src/panes.rs:715` · effort: medium
- **Evidence:** panes.rs:715 `if self.daemon_cfg.is_some() && let Some(ps) = tab.pane_sessions.get(old).filter(|s| s.provider == "daemon")` — with the daemon disabled this whole reattach branch is skipped and the layout falls through to a fresh in-process spawn of the same argv. The previously-persisted daemon sessions keep running under the still-alive daemon with untimed relay leases (`lease_grace_secs: 0` default = "a detached session lives until explicitly killed", config_daemon.rs:26-27, service.rs:104-107), and nothing kills or surfaces them.
- **Impact:** Toggling the daemon off duplicates long-running commands (a dev server now runs twice — the invisible daemon copy still holds its port, so the fresh pane's copy fails confusingly) and orphans shells indefinitely; the only window into them is `thegn session list`, which the user who just disabled the daemon has no reason to run.
- **Fix:** When materializing a layout with `provider == "daemon"` pane_sessions while the daemon route is disabled, either kill those sessions via the control client (if a daemon answers) or emit a status-line warning listing the live detached sessions.
- **Status:** ⏳

### [event-loop] Blocking `write_all` to the PTY master on the loop — a large paste into a non-reading/flow-stopped child hangs the UI indefinitely

- **Where:** `crates/thegn-host/src/pane.rs:662` · effort: medium
- **Evidence:** pane.rs:659-663: `fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> { match &mut self.io { PaneIo::Pty { writer, .. } => { writer.write_all(bytes).context("pty write")?; ... }` — the master writer is a blocking fd, and `write_all` is called directly from the event loop's paste arm (run.rs:17766 `p.write_input(s.as_bytes())?` with the full unbounded clipboard payload) and key dispatch. The kernel PTY input buffer is finite (~KBs); if the child is stopped (SIGSTOP), flow-controlled (Ctrl-S), or simply not reading stdin, `write_all` blocks the compositor thread until the child drains it — potentially forever. The Stream variant directly below (pane.rs:667-668) already handles this correctly: `// Drop on a full/closed control channel rather than blocking the loop` + `try_send`.
- **Impact:** Pasting a large clipboard (or a key-repeat flood) into a pane whose child isn't consuming stdin blocks the entire event loop — no input, no rendering, no other panes serviced — with no recovery until the child reads or dies. Same invariant violation class as the launch_spec finding but triggered by ordinary paste.
- **Fix:** Give the local PTY path the same non-blocking discipline as the Stream path: route pane input through a bounded per-pane writer queue serviced off-thread (mirroring frame_writer), or set the master writer non-blocking and buffer the remainder, surfacing sustained backpressure in `model.status`.
- **Status:** ⏳

### [event-loop] Render-path window-title persistence opens the DB on the loop (5s busy_timeout ceiling) on every title-changing frame

- **Where:** `crates/thegn-host/src/run.rs:9513` · effort: trivial
- **Evidence:** run.rs:9513-9521, inside the `should_render` block executed per frame: `if !title_writes.is_empty() && let Ok(db) = thegn_core::db::Db::open() { for (path, title) in &title_writes { ... db.set_worktree_window_title(path, title); } }`. Each `Db::open` re-runs pragmas plus `conn.busy_timeout(std::time::Duration::from_millis(5000))` (thegn-core/src/db.rs:176). Change-detection limits this to frames where some pane's OSC title actually changed — but shells that stamp the running command into the title (standard zsh/bash precmd/preexec) change it constantly under streaming output, and any program animating its title makes this per-frame. db_task.rs:1-19 documents this exact hazard ("each open re-runs the WAL pragmas plus the user_version migration check (with a 5s busy_timeout ceiling if the write lock is contended)") and provides the off-loop writer these writes should use.
- **Impact:** A DB write-lock held by any concurrent writer (hydration bulk upserts, a `thegn merge`/fold-actor process, a second instance sharing the DB) stalls a render frame — and thus input handling — for up to 5 seconds, on the hot streaming path. This is the highest-frequency member of the known ~55-site on-loop `Db::open` debt (the rest are one-shot user actions).
- **Fix:** Route the title writes through the existing `db_task::persist` fire-and-forget writer (its stated purpose) — the DB is a cache and the write is already best-effort; the in-memory `model.sidebar_window_titles` update stays on the loop.
- **Status:** ⏳

### [event-loop] Drain-side `report_pane_connect_failure` does 3 DB opens + a `git rev-parse` subprocess fallback on the event loop

- **Where:** `crates/thegn-host/src/pty_drain.rs:65` · effort: small
- **Evidence:** pty_drain.rs:64-75, called from `handle_exit` at line 682 (the on-loop drain): `let loc = thegn_core::remote::GitLoc::for_worktree(Path::new(wt));` (which itself does `crate::db::Db::open()` — remote.rs:162), then `let repo_root: PathBuf = thegn_core::db::Db::open().ok().and_then(|db| db.repo_root_for(wt)...).or_else(|| thegn_core::repo::main_worktree(Path::new(wt)))` — `main_worktree` shells out: repo.rs:33 `util::git_out(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])`, then a third `Db::open()` at line 72 for `effective_env`. The function's own comment claims "Cheap resolution ... (DB effective-env; no network)" but on a DB miss it spawns a git subprocess, and each Db::open carries the 5s busy_timeout ceiling.
- **Impact:** Every fast-crash exit of a remote/provider pane runs 3 DB opens plus a possible git subprocess on the loop, exactly during a crash storm — and immediately before the same drain runs the on-loop `launch_spec` respawn (see the P1 finding), compounding the stall. Contention on the shared DB (fold-actor, second instance) turns this into multi-second UI freezes at the worst moment.
- **Fix:** Move the whole provider-unhealthy resolution onto `spawn_blocking` (it ends in `crate::agent::native_exec_report`, an atomic registry write — nothing needs the loop), or at minimum reuse one Db handle and drop the `main_worktree` subprocess fallback on this path.
- **Status:** ⏳

### [panics] Issue 'assign to me' swallows the tracker API failure while showing an optimistic success status

- **Where:** `crates/thegn-host/src/handlers/tracker.rs:194` · effort: small
- **Evidence:** `let _ = rt.block_on(router.update_issue(&issue.id, &patch));` followed by `ctx.model.status = format!("Assigning {} to you…", issue.number);`. The patch is `IssuePatch { assignee_me: Some(true), .. }` — a user-invoked action. If the provider call fails (network, auth, 4xx) the error is discarded with no `model.status`, `msg`, or `tracing`, and the UI has already told the user the assignment is happening. CLAUDE.md's own convention: "never swallow errors on the primary path of a user-invoked action (surface those via model.status, msg, or tracing)".
- **Impact:** User believes an issue was assigned to them when the request silently failed; the subsequent cache refresh shows the old assignee with no explanation. Incorrect state on a user-invoked path.
- **Fix:** Capture the Result; on Err send a status/notification back over the existing waker+channel (the spawn already holds `waker2`), e.g. push a `put_notification` or a status-line message like "assign failed: {e}" instead of the optimistic text.
- **Status:** ⏳

### [races] IPC unix bind_exclusive stale-socket TOCTOU: racing binders can unlink each other's live socket, yielding two 'Bound' daemons

- **Where:** `crates/thegn-svc/src/ipc.rs:214` · effort: medium
- **Evidence:** `match tokio::net::UnixStream::connect(sock).await { Ok(_) => return Ok(BindOutcome::AlreadyRunning), Err(_) => { let _ = std::fs::remove_file(sock); } }` then `UnixListener::bind(sock)`. Interleaving with a stale socket (prior daemon crashed — only graceful shutdown unlinks): A probe-fails and unlinks; B probe-fails; A binds (creates a new file); B's remove*file then deletes A's LIVE bound socket (unlink does not fail on a bound socket); B binds the now-free path and also gets Bound. `ensure_daemon` (daemon/client.rs:65-88) lazily spawns `thegn daemon` on every connect failure, so concurrent spawns racing over one stale socket are the expected trigger. Compounding: daemon/mod.rs:316 `let * = std::fs::remove_file(&sock);` on shutdown unconditionally unlinks whatever is at the path — after an idle-exit/restart race it deletes a successor daemon's freshly-bound live socket.
- **Impact:** Two daemons both believing they own the state dir: duplicate DaemonRow registrations/heartbeats, sessions split between a reachable and an unreachable (unlinked-inode) daemon, discovery rows whose endpoint path is now owned by the other process. This is the production race class behind the flaky svc `ipc::unix_bind` bind-is-the-lock contract: the lock is not atomic on the stale-file path.
- **Fix:** Serialize the probe/unlink/bind critical section under an flock on a sidecar `<sock>.lock` file (O_CREAT, never unlinked). On shutdown, only remove the socket if it is still ours (compare `stat(path).ino` against the listener's bound inode, or skip the unlink entirely and rely on the stale-file probe).
- **Status:** ⏳

### [races] Daemon SessionActor performs blocking PTY writes on the async actor task — a stalled child wedges the actor (Kill/Attach unserviceable)

- **Where:** `crates/thegn-host/src/daemon/session.rs:199` · effort: medium
- **Evidence:** In the actor's async `run()` select loop: `Some(SessionMsg::Stdin(bytes)) => { ... if let Err(e) = self.pty.writer.write_all(&bytes) ... let _ = self.pty.writer.flush(); }`. `writer` is a blocking `Box<dyn Write>` on the PTY master. If the child stops reading stdin and the kernel PTY input buffer fills (~64KB), `write_all` blocks the actor task on a tokio worker thread indefinitely. The actor is the sole consumer of both `pane_rx` and `msg_rx` (module doc, lines 4-9), so while blocked it processes no output, no `Attach`, and — critically — no `SessionMsg::Kill` (session.rs:208) and the lease reaper's Kill (daemon/mod.rs:367) is also just a message.
- **Impact:** One misbehaving child plus a flooding client permanently wedges that session's actor and a runtime worker thread: the session cannot be killed, detached, or reaped, and its PTY reader eventually blocks on the full pane channel too. Denial-of-service of the daemon's session lifecycle from ordinary user input.
- **Fix:** Move PTY writes off the actor: a dedicated writer thread per session fed by a bounded std channel with try_send (drop or error on overflow), mirroring how the compositor pane keeps blocking I/O off the loop; or set the master fd non-blocking and buffer unwritten bytes in the actor.
- **Status:** ⏳

### [security] Pane daemon unix socket grants unauthenticated full admin with no peer-credential check and no socket/dir permission hardening

- **Where:** `crates/thegn-svc/src/control/http.rs:129` · effort: medium
- **Evidence:** `let ctx = if state.local_admin { AuthCtx::local_admin() } else { ... verify token ... }`. `AuthCtx::local_admin()` returns `scopes: ScopeSet::parse("admin")`. The default-on pane daemon builds this state with `local_admin: cfg.serve.local_admin` (daemon/mod.rs:219) and `ServeConfig::default { local_admin: true }` (config_daemon.rs:78). The config comment claims "Unix-socket peers (same uid, via peer credentials) get implicit admin" (config_daemon.rs:69) but there is NO SO_PEERCRED / getpeereid / peer_cred check anywhere in the tree — the admin grant is unconditional for any peer that connects to the listener. The socket is created with default umask via `UnixListener::bind` and its parent is `std::fs::create_dir_all(parent).ok()` with no restrict (daemon/mod.rs:122-124). When XDG_RUNTIME_DIR is unset the socket falls back to `<state_dir>/run/daemon.sock` (config_daemon.rs:53) under an unrestricted ~/.local/state tree.
- **Impact:** On a multi-user host (or any environment where XDG_RUNTIME_DIR is unset, e.g. non-lingering SSH sessions / minimal containers), the daemon socket becomes reachable by other local users, who then get full `admin` on the control API with no token: they can `POST /v1/sessions` / `split` with arbitrary `argv` + `cwd` to run any command as the daemon's user, open worktrees, and git-commit. The 'same uid via peer credentials' guarantee the code documents is not actually enforced — it fails open to admin.
- **Fix:** Enforce peer credentials on the unix listener (SO_PEERCRED / getpeereid, reject non-matching uid) before granting local_admin, and defensively create the socket dir 0700 + chmod the socket 0600 (fsperm::restrict_dir_to_owner is already available). Do not rely solely on ambient filesystem perms.
- **Status:** ⏳

### [security] `thegn serve` default bind is 0.0.0.0:5380 (all interfaces), plaintext HTTP bearer

- **Where:** `crates/thegn-core/src/config_daemon.rs:76` · effort: trivial
- **Evidence:** `bind: "0.0.0.0:5380".into(),` in `ServeConfig::default`. daemon/mod.rs:232 binds this directly: `TcpListener::bind(&bind)`. The module comment (daemon/mod.rs:227) admits "v1 is plaintext: bind to a trusted interface ... or reach it over ssh -L" — but the default does the opposite of that advice.
- **Impact:** A user running `thegn serve` with defaults exposes the control plane on every interface in cleartext. Bearer tokens are required (good), but they traverse the wire in plaintext (sniff/replay) and the port is reachable from the whole LAN/internet if not firewalled. A safe default should be loopback.
- **Fix:** Default `bind` to `127.0.0.1:5380` (or refuse to start on a non-loopback bind without an explicit flag / TLS). Keep 0.0.0.0 opt-in.
- **Status:** ⏳

### [security] Kaneo device-flow bearer token stored in plaintext in the world-readable-by-default SQLite DB

- **Where:** `crates/thegn-core/src/db.rs:612` · effort: small
- **Evidence:** `CREATE TABLE IF NOT EXISTS kaneo_auth ( base_url TEXT PRIMARY KEY, token TEXT NOT NULL, fetched_at INTEGER NOT NULL );`. `put_kaneo_token` inserts the token verbatim (db_cache.rs:66-68) and it is sent as a live credential `Authorization: Bearer {token}` (thegn-svc/src/issue/kaneo.rs:65). The DB is opened via `Connection::open(&path)` with default umask and no `restrict_to_owner`/`restrict_dir` on the file or the `$XDG_STATE_HOME/thegn` dir (db.rs:150-156; no permission call at host startup). This directly violates the codebase's own stated at-rest invariant: db_iroh.rs:14-17 asserts a leak of thegn.db 'yields only hashes, not live tokens'.
- **Impact:** With a typical umask (0022) thegn.db is mode 0644; on a shared host another local user (or any state-dir backup) can read a live Kaneo bearer token and impersonate the user against their Kaneo instance. Control tokens are hashed, so this is the one plaintext-secret-at-rest hole.
- **Fix:** Either hash/encrypt the Kaneo token at the store boundary (as db_iroh does for sandbox tokens) or, at minimum, restrict thegn.db and the state dir to 0600/0700 on open (fsperm helpers already exist).
- **Status:** ⏳

### [security] Control-plane git verbs run against an arbitrary caller-supplied worktree path with no confinement to registered worktrees

- **Where:** `crates/thegn-host/src/daemon/service.rs:415` · effort: medium
- **Evidence:** `git_status`/`git_stage`/`git_commit`/`merge_add` all do `GitLoc::for_worktree(std::path::Path::new(&wt))` on the raw request field and run git there (e.g. line 422-423, 449-451, 470-471). No check that `wt` is one of thegn's known worktrees. The HTTP layer only gates by scope (`Verb::GitCommit` → `git`), never by path.
- **Impact:** A holder of a narrowly-scoped `git` token (or any local_admin peer) can stage/commit in ANY git repository path on the host, not just the user's thegn worktrees — a confused-deputy escape of the intended scope. (Option-injection is mitigated: stage/discard use `git add/checkout -- <path>`.)
- **Fix:** Validate the `worktree` argument against the DB worktree registry (or a canonicalized allowlist under worktrees_dir) before running any git mutation; reject unknown paths with NotFound.
- **Status:** ⏳

### [session-persist] Quit/Detach kills daemon-backed sessions of parked (resident-pool) workspaces — persistence only survives for the active workspace

- **Where:** `crates/thegn-host/src/handlers/daemon_lifecycle.rs:30` · effort: small
- **Evidence:** `mark_session_panes_detached` walks only `for g in &session.worktrees` (the ACTIVE workspace). All quit paths call exactly this — run.rs:15571-15578 (`Action::Quit | Action::Detach`), run.rs:13043-13047 (palette "quit"), run.rs:6658-6664 (SIGTERM) — while workspaces parked in the `WorkspacePool` keep live panes in `panes.table` that are NOT in `session.worktrees` and were never flagged: `WorkspacePool::stash` (workspace_pool.rs:95-117) only calls `detach_pane` on eviction/limit-0, and grep shows no other `set_detach_on_drop` caller. On process exit those panes drop with the kill-on-drop default, and the relay explicitly kills the server-side session: pane.rs:1018-1020 `if !detach_on_drop.load(…) && … source.kill_session(&sid).await` (SessionEnd::PaneGone, 'Unless it was marked detached … kill the server-side session'). Whether the kill lands before the runtime tears down is a race, so the outcome is nondeterministic.
- **Impact:** With `[daemon] enabled`, a user who visited workspace A, switched to workspace B, and quits loses (racily) every daemon-backed shell/process in A — plus any terminal groups resident in A's parked session — contradicting 'quit is a detach; daemon-backed sessions keep running'. The 'kept N sessions running' exit line also undercounts (KEPT_SESSIONS only counts active-session panes). QuitKill is unaffected (kill_daemon_sessions_blocking iterates the whole `panes.table`).
- **Fix:** On the Quit/Detach and SIGTERM paths, mark detach-on-drop for every center-tree pane of every parked `ResidentWorkspace` in the pool as well (e.g. give `WorkspacePool` an `iter_pane_ids()` and extend `mark_session_panes_detached` to take it), or simply mark every daemon-backed pane in `panes.table` that belongs to any persisted center tree.
- **Status:** ⏳

### [session-persist] A remote terminal whose connection dies after 2s is misclassified as an interactive close and its terminals registry row is deleted

- **Where:** `crates/thegn-host/src/pty_drain.rs:653` · effort: small
- **Evidence:** `let interactive_close = !shutting_down && age >= CRASH_THRESHOLD;` then `close_exited_terminal(ctx, gi, ti, interactive_close)` — the decision to DELETE the durable `terminals` row (pty_drain.rs:909-919 `db.del_terminal(id)`) is based purely on pane age (>= 2s) and the shutdown flag; `exit_code` is received by `handle_exit` but never consulted for terminals. An ssh/mosh terminal whose transport dies any time after the 2-second window — network drop, remote reboot, laptop sleep, VPN flap (ssh exits 255) — therefore takes the 'genuine interactive close (Ctrl-D on a live terminal)' branch and the saved terminal (name, connection string, sandbox backend, env) is permanently deleted from the registry.
- **Impact:** Silent loss of user-configured remote terminals on a non-user event. The commit 61dcaa91 fix ('persist terminals across sessions') covers fast crashes (<2s) and shutdown, but any longer-lived connection failure still reaps the row — precisely the terminals (remote ones) the persistence feature matters most for. Inverse mild wart: a genuine Ctrl-D within 2s of opening keeps the row and the terminal resurrects.
- **Fix:** Require a clean exit for registry deletion: `interactive_close = !shutting_down && age >= CRASH_THRESHOLD && exit_code == Some(0)` (pass `exit_code` through to `close_exited_terminal`). A non-zero/unknown exit keeps the row so the terminal re-materializes as an inactive entry, matching the fast-crash keep-case.
- **Status:** ⏳

### [test-gaps] sandbox_tests secrets tests fail under ambient THEGN_SANDBOX=1 (EnvGuard doesn't clear it)

- **Where:** `crates/thegn-core/src/sandbox_tests.rs:1197` · effort: trivial
- **Evidence:** `let _env = crate::testenv::EnvGuard::set(&[("GH_TOKEN", "ghp_secret")]);` then line 1202 `("THEGN_SANDBOX".into(), "1".into()), // synthetic ⇒ inline` and line 1228 asserts `j.contains("-e THEGN_SANDBOX=1")`. But `partition_secret_env` (sandbox.rs:1670) classifies any pair whose value matches the launcher env as secret: `if local && std::env::var(k).ok().as_deref() == Some(val) { secret.push(...) }` — and every thegn sandbox exports THEGN_SANDBOX=1 (sandbox.rs:628). Same bug in `systemd_local_secrets_go_to_environment_file_not_argv` (line 1236, assert at 1260-1262). Other tests in this file already guard it (line 501 `EnvGuard::unset(&["THEGN_SANDBOX"])`) — these two only guard the token var.
- **Impact:** Running `just test` / the pre-push hook inside a live thegn sandbox (the documented dev mode: "This shell often runs inside a live thegn") turns the gate spuriously red on these two tests — confirmed failing today. Devs learn to bypass the pre-push gate, which is the single heavy gate before code leaves the machine. Also breaks `just coverage` (cargo llvm-cov runs thegn-core via plain cargo test).
- **Fix:** Change both guards to `EnvGuard::mutate_pairs(&[("GH_TOKEN", Some("ghp_secret")), ("THEGN_SANDBOX", None)])` (mutate_pairs exists exactly for this — ENV_LOCK is not reentrant, so don't stack two guards).
- **Status:** ⏳

### [test-gaps] Daemon pane resurrect-at-startup (warm-reattach + fallback restore) untested — the headline persistence feature

- **Where:** `crates/thegn-host/src/panes.rs:710` · effort: medium
- **Evidence:** `// Pane-daemon warm-reattach: a persisted \`provider = "daemon"\` session means this leaf's process may still be alive...`— the whole branch in`materialize_with_specs`(lines 710-752), including`p.set_fallback_restore(tab.pane_scrollback.get(old).cloned(), ...)`, has no test (panes.rs' 17 tests cover argv/shell resolution, not this branch), and the loop-side application of the restore payload on `PaneEvent::SessionFallback` (run.rs ~17532: "A pane offering a relaunch (resurrected with a remembered...") is untested. pane.rs:1581 tests only that a dead attach EMITS SessionFallback.
- **Impact:** This is 'terminals persist across sessions' (commit 61dcaa91), the feature the daemon exists for. A regression here ships as: after quitting/reopening the UI, panes come back as error husks instead of warm-reattached terminals, or after a reboot (daemon gone) panes lose their persisted scrollback tail and relaunch hint. None of it is caught by smoke (which never restarts a compositor against a live daemon) or unit tests.
- **Fix:** A panes-level test that seeds `tab.pane_sessions` with a `provider = "daemon"` entry pointing at (a) a live in-process daemon session — assert reattach, no fresh spawn; and (b) a bogus session id — assert the pane surfaces SessionFallback and that the stashed scrollback/cmd restore payload is taken via `take_fallback_restore`. Reuses the harness from finding #1.
- **Status:** ⏳

### [test-gaps] thegn land / thegn integrate CLI wrappers have zero smoke or unit coverage despite mutating refs/heads/main

- **Where:** `crates/thegn-host/src/cmd/land.rs:49` · effort: small
- **Evidence:** `pub fn run(cfg: &Config, worktree: Option<String>) -> Result<()> { ... if let Some(msg) = crate::merge_ops::remote_target_guard(&db, &root) { outln!("{msg}"); return Ok(()); }` — cmd/land.rs (81 LOC) and cmd/integrate.rs (119 LOC) have 0 tests, and `grep 'thegn land\|integrate' test/smoke.sh` finds nothing (smoke covers `merge add/rm/drain` but never the one-shot `land`/`integrate` verbs). merge_ops.rs (`remote_target_guard`, `enqueue_worktree`) is also 0-test, 125 LOC. The core `attempt_land` is well-tested (integrate.rs, 12 tests) — only the CLI wiring above it is dark.
- **Impact:** `thegn land` is the blessed way to land onto main from a read-only sandbox (per CLAUDE.md). A wrapper regression — e.g. `remote_target_guard` misfiring so `run` prints a message and returns Ok(()) WITHOUT landing, or branch resolution picking the wrong worktree — ships undetected and users' 'landed' branches silently never reach main (or exit 0 while doing nothing).
- **Fix:** Add a smoke check mirroring the merge-drain block: create a worktree, commit, `"$SZ" land "$WT"`, assert the commit is in the target's log and the success line printed; plus one negative (detached HEAD ⇒ clear error, exit non-zero).
- **Status:** ⏳

### [ux-polish] OnboardingWizard::new does a synchronous OS-keyring write probe on the launch path and on the event loop

- **Where:** `crates/thegn-host/src/onboarding.rs:387` · effort: small
- **Evidence:** onboarding.rs:387 `keyring: crate::secret::keyring_available(),` inside `OnboardingWizard::new`. secret.rs:70-83: `keyring_available` does a live round-trip — `e.set_password("1")` then `delete_credential()` against the Secret Service. Constructor call sites: run.rs:6078 (`handlers::onboarding::startup`, which runs before the first frame is flushed at run.rs:10363 — and this is exactly the first-run path where the wizard auto-arms) and run.rs:13240 (palette "setup-wizard" dispatch, executed directly on the event loop).
- **Impact:** On a fresh machine (the one case where the wizard always constructs) a locked/slow/activatable-but-absent Secret Service daemon can block launch for seconds (D-Bus activation timeout, or a keyring-unlock prompt the user can't see behind the alt screen), violating both the sub-300ms first-frame goal and the "never put blocking I/O on the loop" invariant on the palette re-run path. The probe only feeds the cosmetic "tokens → OS keyring / 0600 file" note (onboarding.rs:1266-1270).
- **Fix:** Default `keyring` to false (or `Option<bool>` = unknown) and resolve it via the existing off-thread probe machinery (`spawn_probe` / `RefreshKind::Onboarding`), updating the note when the answer lands — same pattern as the Forge/Sandbox probes.
- **Status:** ⏳

## P3

### [cli-api] User-facing hints recommend hidden legacy spellings (`repo-trust`, bare `clean`)

- **Where:** `crates/thegn-host/src/cmd/repos.rs:159` · effort: trivial
- **Evidence:** `outln!("approve with: thegn repo-trust {} --approve <id>", root.display());` and line 126 `"no pending request with id {id:?}; run `repo-trust {}` to list"` — `repo-trust` is `#[command(hide = true)]` (main.rs:331-333); canonical is `thegn repo trust`. Same class: disk.rs:104 `"Reclaimable (target/): {} — `thegn clean --all` to recover."` recommends the hidden legacy `clean` instead of `wt clean`.
- **Impact:** First-run users follow the hint into commands that don't appear in `--help` or completions, undermining the noun-verb grammar the alpha is presenting; wt.rs's own docs say the legacy verbs are hidden compatibility shims.
- **Fix:** Change the hint strings to `thegn repo trust …` and `thegn wt clean --all`.
- **Status:** ⏳

### [cli-api] `merge rm` prints "Removed from queue." even when nothing was queued

- **Where:** `crates/thegn-host/src/cmd/merge.rs:214` · effort: small
- **Evidence:** `db.remove_merge_entry(&wt.to_string_lossy())?; outln!("Removed from queue.");` — `remove_merge_entry` (thegn-core/src/db_aux.rs:199-205) is a bare `DELETE FROM merge_queue WHERE worktree=?1` that ignores the affected-row count.
- **Impact:** Running `thegn merge rm` in the wrong directory (or with an already-removed worktree) reports success; the user believes a queued branch was pulled from the queue when the real row (keyed by another path) is still there and will be folded on the next drain.
- **Fix:** Return the row count from `remove_merge_entry` and print "not in the queue" + exit EXIT_NOT_FOUND when 0 rows matched.
- **Status:** ⏳

### [cli-api] Inconsistent worktree-targeting shape: `--worktree` flag vs positional across sibling verbs

- **Where:** `crates/thegn-host/src/main.rs:295` · effort: medium
- **Evidence:** `Land { /// Worktree path (default: the current worktree). worktree: Option<String>, }` (positional) — likewise `merge rm/land`, `env show/set/up/down/…`, `sandbox-argv` take positional worktrees, while `pr`/`issue`/`ci`/`wt diff|disk|clean`/`share`/`forward` all take `#[arg(long)] worktree: Option<String>` (e.g. pr.rs:23-25). `wt rm` uses a positional named `target` (wt.rs:96).
- **Impact:** The same concept is spelled two ways depending on the namespace; users must remember per-verb whether it's `thegn land <path>` or `thegn pr status --worktree <path>`. Alpha is the last cheap moment to unify before the shapes become the stable scripting API docs/cli.md promises.
- **Fix:** Pick one convention (accepting a positional AND keeping `--worktree` as an alias is backward-compatible: add `#[arg(long)]` fallbacks to the positional verbs or positionals to the flag verbs) and document it in docs/cli.md.
- **Status:** ⏳

### [cli-api] `wt clean` per-target failures and `wt rm` abort exit 0

- **Where:** `crates/thegn-host/src/cmd/disk.rs:169` · effort: small
- **Evidence:** `Err(e) => outln!("failed to clean {path}: {e}")` inside the loop, then the function ends `Ok(())` — a run where every clean failed still exits 0. Similarly wt.rs:300-302: declining the `wt rm` confirmation prints `outln!("aborted"); return Ok(());` (exit 0).
- **Impact:** Scripts using `thegn wt clean --all --force` to reclaim disk cannot detect that nothing was reclaimed (e.g. permission errors); a declined destructive prompt is indistinguishable from a completed removal by exit code.
- **Fix:** Track failures in `clean` and return an error (or exit EXIT_ERROR) when any target failed; exit non-zero (conventionally 1) on `wt rm` prompt decline.
- **Status:** ⏳

### [config-db] user_version stamped before the schema batch and the post-batch v46 cleanup — a crash in between permanently skips version-gated post-steps

- **Where:** `crates/thegn-core/src/db.rs:226` · effort: small
- **Evidence:** if ver < SCHEMA_VERSION { conn.pragma_update(None, "user_version", SCHEMA_VERSION)?; } runs (autocommitted) BEFORE the CREATE batch (db.rs:238) and before the `if ver < 46 { UPDATE notifications SET read=1 ... }` cleanup (db.rs:637-642), which is "Gated on the pre-bump on-disk version so it runs exactly once". A crash/kill between the stamp and the cleanup leaves user_version=48 on disk, so the next open computes ver=48 and the v46 step never runs — same trap for any future ver-gated post-batch migration.
- **Impact:** Low today (the v46 step only clears a cosmetic notification pile), but the ordering makes every future post-batch gated migration silently skippable on one ill-timed crash.
- **Fix:** Move the pragma_update to the END of init (after the batch, additive_schema, versioned migrations, and gated cleanups), or stamp inside the same transaction as the final gated step.
- **Status:** ⏳

### [config-db] config.toml.example still references the excised managed-pi agent in the [[agents]] prose

- **Where:** `config/config.toml.example:694` · effort: trivial
- **Evidence:** # else its command's program basename — kinds dedup (e.g. a managed-pi "Agent" + \n# a "Vanilla Pi" → one `pi`). — commit b6cd27ef removed "bouncer/ACP, managed pi, and the sealed agent-profile container", and 85f3d1fb's message confirms "config.toml.example: [llm_proxy], managed-pi entry, agent_profile gone", but this dedup example still cites the removed managed-pi feature.
- **Impact:** Public-alpha reference doc points users at a feature that no longer exists; the drift test (crates/thegn-core/tests/config_example.rs) only checks struct→example, so prose like this is not gated.
- **Fix:** Reword the dedup example to use two surviving entries (e.g. two claude-provider entries → one `claude`).
- **Status:** ⏳

### [daemon-separation] Daemon open() inserts the session entry after spawning the actor — a fast-exiting child can leave a permanent ghost session that also blocks idle-exit forever

- **Where:** `crates/thegn-host/src/daemon/service.rs:221` · effort: small
- **Evidence:** service.rs:215 `tokio::spawn(actor.run(pane_rx, msg_rx));` runs before service.rs:221-224 `self.sessions.lock().await.insert(id, SessionEntry {...})`. The actor's teardown removes itself: session.rs:236 `self.sessions.lock().await.remove(&self.meta.id);`. If the child exits (or the reader thread EOFs) fast enough that teardown wins the race, the subsequent insert re-adds an entry whose actor is gone: it is listed forever, `kill` no-ops (`let _ = tx.send(SessionMsg::Kill)`, service.rs:318, receiver dropped), and `idle_exit_loop`'s `!svc.sessions...is_empty()` (mod.rs:409) stays busy forever, so the daemon never idle-exits.
- **Impact:** A narrow but real race (instant-exit argv, e.g. a typo'd command via the HTTP open verb) permanently wedges the daemon's session table and idle-exit janitor until the daemon is manually killed.
- **Fix:** Insert the SessionEntry into the map before `tokio::spawn(actor.run(...))` (the actor's teardown removal then always observes it), or have the actor tolerate remove-then-insert by re-checking after insert.
- **Status:** ⏳

### [daemon-separation] Observer attaches cancel and refresh the relay lease, contradicting the AttachKind contract — a watching observer can keep a doomed session alive indefinitely

- **Where:** `crates/thegn-host/src/daemon/service.rs:254` · effort: small
- **Evidence:** mod.rs (svc control) :66-67 documents: "`Observer` never resizes the PTY and never holds the relay lease open". But service.rs:253-254 runs `// Attaching cancels the relay grace period. self.on_session_busy(session).await;` for every AttachKind, and session.rs:339 counts observers in `attached`, so the last observer's detach re-opens a lease with a FRESH `relay_expiry(now, grace)` (service.rs:107).
- **Impact:** With `lease_grace_secs > 0`, `thegn session attach` (observer, cmd/session.rs:221 passes `observer=true`) on a detached session resets its reap countdown, and a long-lived observer prevents reaping entirely — sessions the operator expects to expire persist. (Invisible with the default grace 0.)
- **Fix:** Skip `on_session_busy` for `AttachKind::Observer` and exclude observers from the idle/busy `attached` count used by `after_sub_change` (track interactive subscribers separately).
- **Status:** ⏳

### [daemon-separation] Every reconnect/lag-resync replays up to 2000 history lines into the client pane as ordinary output, duplicating scrollback per blip

- **Where:** `crates/thegn-core/src/term_snapshot.rs:117` · effort: medium
- **Evidence:** term_snapshot.rs:114-119: `out.extend_from_slice(s.history_tail.replace('\n', "\r\n").as_bytes());` — the snapshot's history tail (SNAPSHOT_HISTORY_LINES = 2_000, daemon/session.rs:38) is emitted as literal output before the `\x1b[2J` repaint. The relay's transient-drop path re-attaches and re-applies a full snapshot (pane.rs:1050-1055), and the lag path sends a resync snapshot mid-stream (session.rs:286-295); both append the entire history tail to the client emulator's scrollback again.
- **Impact:** After a few socket blips or a lag-resync during a flood, the pane's local scrollback contains the same 2000 lines repeated — confusing during scrollback/copy-mode and unbounded growth over many reconnects. (pane.rs:863 acknowledges the re-flood concern for pacing, but not the duplication.)
- **Fix:** Omit `history_tail` from reconnect/resync snapshots (only the initial warm attach needs it), e.g. an `include_history` flag on the snapshot request, or have the client suppress history application when its emulator already has content for the session.
- **Status:** ⏳

### [daemon-separation] EventFrame::Pairing is never emitted by any producer — require_approval pairings park silently with no notification path

- **Where:** `crates/thegn-svc/src/control/http.rs:859` · effort: small
- **Evidence:** The only non-test constructions of `EventFrame::Pairing` are serializers (http.rs:859 `frame_json`, grpc.rs conversions). The `pair` (http.rs:158-183), `issue_pairing`, `approve_pairing`, and `revoke_pairing` handlers only touch the store — none call `api`/`events` to broadcast a Pairing frame, so the wire variant defined in control_wire.rs:132-137 has no producer.
- **Impact:** With `[serve] require_approval = true`, a redeemed token parks (`approved_at = None`) and nothing notifies the operator on the event feed or in the compositor; the phone appears broken until someone thinks to run `thegn pair` and `thegn pair approve`. Dead protocol surface shipped in a public alpha.
- **Fix:** Emit `EventFrame::Pairing { state: Requested }` from the redeem path (and Approved/Revoked from their handlers) via the daemon's broadcast sender, so feeds/UI can surface pending approvals.
- **Status:** ⏳

### [daemon-separation] Daemon-backed panes report exit code 0 when the real code is unknown (killed/reaped sessions look like clean exits)

- **Where:** `crates/thegn-host/src/daemon/client.rs:186` · effort: small
- **Evidence:** daemon/client.rs:185-187: `Some(EventFrame::SessionExit { code, .. }) => { let _ = out_tx.send(ExecFrame::Exit(code.unwrap_or(0))).await; }`. The daemon sends `code: None` exactly when the exit status is unreapable — including the Kill/mailbox-closed teardown (session.rs:208 breaks with `(None, false)`) and lease reaps. The wire doc (control_wire.rs:142 "`code` is `None` when unreapable") is erased at this adapter.
- **Impact:** A session killed by the lease reaper or `session kill` renders in the pane as a clean exit-0, hiding the abnormal termination from the user and from any exit-code-sensitive pane logic (in-process PTY panes propagate the real code).
- **Fix:** Map `code: None` to a distinct non-zero sentinel (or thread `Option<i32>` through `ExecFrame::Exit` so the husk can say "terminated").
- **Status:** ⏳

### [event-loop] `persist_session_layout` (self-documented ~500ms heavyweight DB persist) runs synchronously inside the PTY drain when a terminal's shell exits

- **Where:** `crates/thegn-host/src/pty_drain.rs:922` · effort: small
- **Evidence:** pty_drain.rs:922 `crate::run::persist_session_layout(ctx.session, ctx.panes);` in `close_exited_terminal`, executed from the on-loop drain whenever a standalone terminal's shell exits (including background terminals). The function (run.rs:5129-5136) captures whole-session pane state then `Db::open()` + `session.persist(...)` synchronously; run.rs:5141-5147 quantifies it: "the heavyweight `persist_session_layout` (whole-session scrollback capture + full layout rewrite + a DB open/write/checkpoint-fsync, all on the loop, cost scaling with session size: ~500ms in a debug build on a populated session)".
- **Impact:** A background terminal dying (ssh drop, remote backend gone) stalls the loop for the full persist on a populated session — the user did not invoke anything, so the freeze is unexplained. The other ~18 call sites are explicit user structural actions where a one-shot cost is the accepted trade-off; this one fires from the drain.
- **Fix:** For the drain path, capture the pane state on the loop (cheap, in-memory) and ship the DB open/write to `spawn_blocking` or the db_task writer, mirroring `persist_active_focus` (run.rs:5148-5160) which was split off for exactly this reason.
- **Status:** ⏳

### [event-loop] Dead `orphan_rx` channel: receiver dropped at startup while the GC task still sends and pulses the waker

- **Where:** `crates/thegn-host/src/run.rs:736` · effort: small
- **Evidence:** run.rs:710 `let (orphan_tx, orphan_rx) = tokio_mpsc::unbounded_channel::<Vec<String>>();`, run.rs:730-731 (in the startup-GC `spawn_blocking`) `let _ = orphan_tx.send(removed); let _ = gc_waker.wake();`, and run.rs:735-736 `// orphan_rx drained in the event loop to surface the notice in the System panel.` / `let _ = orphan_rx; // placeholder until wired into event_loop` — the receiver is dropped immediately, so the send always fails and the waker pulse is an orphaned wake the loop services as an empty (Skip) frame. Introduced by commit f8e594be (predates the AI excision), and the comment above it falsely claims the drain exists.
- **Impact:** The "removed N orphan container(s)" notice never reaches the UI (only the log via msg::info), the comment misdocuments the loop, and the pulse is a wasted wake. Minor, but it is exactly the dead-channel shape a future refactor could copy.
- **Fix:** Either wire a `while let Ok(removed) = orphan_rx.try_recv()` drain that posts a toast/status like the other startup notices, or delete the channel and the waker pulse and keep the log line only.
- **Status:** ⏳

### [event-loop] Post-excision stale operator guidance: reverse-tunnel warning still advertises the removed LLM "proxy :8383" tunnel

- **Where:** `crates/thegn-host/src/revtunnel.rs:181` · effort: trivial
- **Evidence:** revtunnel.rs:180-184: `thegn_core::msg::warn("reverse tunnels (nix cache :8484 / proxy :8383) disabled: no resident bridge binary — ...")`. The `:8383` tunnel was the LLM-proxy reverse tunnel removed in the excision (commit bcb6af8a deleted the `cfg.llm_proxy.remote_tunnel_port()` push in `connect_worktree_bridge`; only the nix-cache and `[sandbox.home] reverse_forwards` tunnels remain — run.rs:1561-1590).
- **Impact:** A user hitting the missing-bridge warning is told a proxy tunnel exists that the release no longer ships — confusing operator guidance in a public alpha, and a residual reference to the excised AI layer.
- **Fix:** Reword the warning to name only the remaining tunnels, e.g. "reverse tunnels (nix cache :8484 / [sandbox.home] reverse_forwards) disabled: ...".
- **Status:** ⏳

### [panics] thegn daemon --serve: transient DB read error is swallowed then converted into a guaranteed panic (`expect("own daemon row")`)

- **Where:** `crates/thegn-host/src/daemon/mod.rs:243` · effort: trivial
- **Evidence:** `let mut row = db.daemons().unwrap_or_default().into_iter().find(|d| d.daemon_id == daemon_id).expect("own daemon row");` (lines 238-243). The daemon registers its row with `db.put_daemon(...)?` at line 170-182 (so the row exists), but when re-reading to record the TCP addr, a failed `daemons()` read (e.g. SQLITE_BUSY from a concurrent thegn instance — CLAUDE.md notes "this shell often runs inside a live thegn") is masked by `unwrap_or_default()` into an empty Vec, making the `.expect()` fire unconditionally. The row can also be deleted between write and read by another daemon's boot sweep (`del_daemon` at line 166) if `pid_alive` misjudges.
- **Impact:** Panic during `thegn daemon` serve startup on a user-invoked path, in a function that already returns `anyhow::Result` — the transient DB error should degrade or propagate, not crash.
- **Fix:** Replace `unwrap_or_default()` + `expect()` with `?` + `.context(...)`, or fall back to constructing the row locally (all fields are in scope) and `put_daemon` it with the tcp_addr set.
- **Status:** ⏳

### [panics] Blame fetch swallows git spawn failure and never checks exit status — user-invoked blame view degrades to empty with zero feedback

- **Where:** `crates/thegn-host/src/run.rs:2465` · effort: small
- **Evidence:** `.output().unwrap_or_else(|_| std::process::Output { status: std::process::ExitStatus::default(), stdout: Vec::new(), stderr: Vec::new() })` in `spawn_blame_fetch` (run.rs:2463-2469). A spawn failure or a git error (untracked file, bad rev — nonzero exit, stderr ignored) yields empty stdout → `parse_blame_porcelain` returns `[]` → `GitDoc::Blame(vec![])` is sent as if it were a successful empty result. No `tracing`, no status message. This is the primary path of the user-invoked blame view, not a best-effort cache write.
- **Impact:** Opening blame on a file where git fails shows an empty blame document indistinguishable from success; the user gets no error and no way to diagnose.
- **Fix:** Check `output.status.success()`; on failure send a distinct error variant (or reuse the doc channel with an error string from stderr) so the panel can render "blame failed: …", and `tracing::warn!` the spawn error instead of fabricating a default Output.
- **Status:** ⏳

### [races] Active-tab pointer has unordered concurrent writers: spawn_blocking focus persists race each other and the synchronous full layout persist

- **Where:** `crates/thegn-host/src/run.rs:5157` · effort: trivial
- **Evidence:** `persist_active_focus` runs `tokio::task::spawn_blocking(move || { ... Session::persist_active_tab(&db, &sid, &name, ...) })` (run.rs:5148-5163). tokio's blocking pool runs tasks concurrently with no ordering, so two rapid Alt+Up/Down switches can commit their `set_active_tab` writes in reverse order (older name wins). The same row is also written synchronously on the loop by `Session::persist` → `db.set_active_tab(...)` (session.rs:491-493) on structural changes and shutdown, which an in-flight blocking task can overwrite after.
- **Impact:** Last-writer-wins on the `session_state` active-tab row is nondeterministic under rapid switching: the next cold start (or workspace switch-back) restores focus to the wrong worktree/tab. Low stakes (focus only, no data loss) but a genuine two-writers-same-row race on a user-visible restore path.
- **Fix:** Route the focus persist through the existing db_task FIFO writer thread (`crate::db_task::persist`) instead of spawn_blocking — send order then equals commit order and it also serializes against nothing-else-on-that-thread; or carry a loop-side monotonic sequence and skip the write if a newer one was queued.
- **Status:** ⏳

### [races] Corner-pane loop_fed flip races the PTY reader: early output can be parsed twice into the emulator

- **Where:** `crates/thegn-host/src/run.rs:8702` · effort: small
- **Evidence:** Pin/corner respawn: `panes.spawn_argv_env_local(...)` then `if let Some(p) = panes.table.get(&fresh) { p.set_loop_fed(true); }` (also pty_drain.rs:578-580). The pane spawns with `loop_fed = false` (pane.rs:310) so the reader thread parses each chunk via `sink.advance(&buf[..n])` when `!loop_fed.load(Relaxed)` (pane_pty.rs:129-134) AND sends the raw bytes on the channel. A chunk read between spawn and `set_loop_fed(true)` is advanced on the reader; when the loop later drains that same chunk, `PtyPane::feed` sees `loop_fed == true` and advances the emulator again (pane.rs:544-545).
- **Impact:** The first output burst of a corner overlay pane (prompt/banner of btop-style daemons, which emit immediately) can be double-parsed: duplicated text or corrupted escape state in the corner grid until the next full repaint. Narrow window, cosmetic corruption, but a real cross-thread flag/data race by construction.
- **Fix:** Spawn corner/pin panes loop-fed from the start: add a spawn variant that passes no feed sink (or seeds the AtomicBool true before open_pty) instead of flipping the flag after the reader thread is already running.
- **Status:** ⏳

### [races] Daemon open() spawns the session actor before inserting its entry — instant-exit children can leave a ghost session that blocks idle-exit

- **Where:** `crates/thegn-host/src/daemon/service.rs:215` · effort: small
- **Evidence:** `tokio::spawn(actor.run(pane_rx, msg_rx));` at line 215 precedes `self.sessions.lock().await.insert(id, SessionEntry { ... })` at lines 221-224. The actor's teardown removes itself: `self.sessions.lock().await.remove(&self.meta.id);` (daemon/session.rs:236). If the child EOFs immediately (exec failure inside the PTY, `sh -c true`-style command) and the actor task is scheduled first, the remove runs before the insert — the entry is then inserted for an already-dead actor and is never removed.
- **Impact:** Ghost session entry: listed forever by list_sessions, attach fails oddly (closed channel), and `idle_exit_loop` (daemon/mod.rs:409) sees `sessions` non-empty forever, so an otherwise-idle daemon never exits — an orphan daemon per state dir until manually killed.
- **Fix:** Insert the SessionEntry into the map BEFORE spawning the actor task (the actor only needs the map handle for teardown, which then always observes its own entry); or have the actor await a one-shot 'registered' signal before its select loop.
- **Status:** ⏳

### [races] integrate.rs gate tests run the reuse-mode gate against the user's real $XDG_STATE_HOME and leak orphan gate worktrees

- **Where:** `crates/thegn-host/src/integrate.rs:887` · effort: trivial
- **Evidence:** The test `cfg()` helper uses `..MergeQueueConfig::default()` which leaves `gate_reuse_worktree: true` (thegn-core/src/config.rs:485). Tests like `green_gate_advances_red_gate_holds_back` and `attempt_land_reports_gate_failure_and_holds_main` therefore call gate_tip's reuse path, which writes worktrees under `util::xdg_state_home().join("thegn/gate")` (integrate.rs:353-355) — the developer's live state dir, violating the repo's own rule that tests must isolate XDG_STATE_HOME. `Repo::drop` deletes the fixture repo but never the reused gate worktree (only non-reuse worktrees are removed, integrate.rs:442-444), so each test run permanently accumulates orphaned `sz-integ-*` checkout dirs (with dangling .git pointers) in the user's state dir.
- **Impact:** State-dir pollution growing per test run, and the only cross-test shared mutable state the flaky attempt_land tests touch under parallel load (concurrent suite runs all churn `$XDG_STATE_HOME/thegn/gate/`). Note the observed 2026-08-14 panic itself is most consistent with a git subprocess failing under load and surfacing via the tests' `.unwrap()` (all in-test repos and gate dirs are uniquely named per pid+tag), so this is the hygiene fix, not a proven root cause.
- **Fix:** Set `gate_reuse_worktree: false` in the tests' cfg() (the throwaway /tmp path is already concurrency-safe and self-cleaning), or point XDG_STATE_HOME at a per-test temp dir for the gate-using tests.
- **Status:** ⏳

### [security] Repo `.thegn.*` [notifications.sound] overlay carries an ungated shell `command` that reaches `sh -c` (latent — not wired to a repo_root at runtime today)

- **Where:** `crates/thegn-core/src/config.rs:3102` · effort: small
- **Evidence:** `NotificationsOverlay::apply` overwrites `base.sound = v;` (config.rs:3102) with the repo-supplied `SoundConfig` — including `mode = "command"` + `command = "..."` and per-rule `sound` strings — with NO clamp/gate (contrast the carefully clamped sandbox overlay in config_resolve and the trust-gated devcontainer path). `effective_notifications` applies the repo overlay when given a repo_root (config.rs:4199-4204), and a `SoundMode::Command` resolves to `SoundEmit::Command` → `spawn_sound_command` → `Command::new("sh").arg("-c").arg(&cmd)` (notify.rs:170, 289-291). The only thing preventing repo-controlled RCE-on-notification today is that the runtime NotifyState is built with `effective_notifications(None)` (run.rs:6046, 8863), so the repo layer is never folded in at runtime.
- **Impact:** No live exploit as traced (runtime callers pass None), but this is a loaded gun: a hostile repo's `[notifications.sound] command` is parsed and would execute arbitrary shell via sh -c the moment any caller passes a repo_root to effective_notifications. Every sibling repo-controlled channel (sandbox, devcontainer, issues) is gated; this one is not.
- **Fix:** Drop/deny the `command` sound mode and per-rule command `sound` when they originate from a repo overlay (strip in NotificationsOverlay before apply, or route the repo notification overlay through a clamp), so a repo can never supply an executable command string regardless of which call site folds it.
- **Status:** ⏳

### [session-persist] Orphaned terminal group silently degrades a remote/sandboxed terminal to a local uncontained shell

- **Where:** `crates/thegn-host/src/run.rs:4646` · effort: small
- **Evidence:** `terminal_launch_for` doc + body: 'Returns `("", "")` when the row is gone or the DB can't be opened — a safe fall back to a local, uncontained shell.' A terminal GROUP persists per-session in `tab_groups` while its connection lives in the global `terminals` table; the two can desync: a second thegn instance sharing the state dir deletes the terminal (`close_terminal` → `del_terminal`) while this instance's session still holds the group and re-persists it, or the deferred `spawn_blocking` delete races a persist. On the next resurrect+activation, `handlers/materialize.rs:143` `let (conn, sandbox) = crate::run::terminal_launch_for(&gname);` gets `("", "")` and `terminal_launch_spec(cfg, "", "")` opens a plain LOCAL uncontained shell under the terminal's name — same for a transient `Db::open` failure at materialize time.
- **Impact:** A tab labeled with the saved ssh/sandboxed terminal's name opens a local, unsandboxed shell on the host; a user can type remote-intended commands locally. The tabbar env chip does show [local] (hydrate_terminal.rs:56-70), which mitigates but does not prevent it.
- **Fix:** Distinguish 'row missing' from 'row is local': have `terminal_launch_for` return `Option`, and on `None` fail the materialize with a status ('terminal "X" no longer exists — recreate it') instead of building a local-shell spec; only a DB-confirmed `kind == local` row should produce a local shell.
- **Status:** ⏳

### [session-persist] drain_specs addresses the target tab by stale index: a tab close during an in-flight materialize can inject/leak panes into the wrong tab

- **Where:** `crates/thegn-host/src/handlers/provision.rs:434` · effort: medium
- **Evidence:** `let Some(tab) = ctx.session.worktrees[gi].tabs.get_mut(ti) else { continue; };` — the batch key is `(group name, tab index)` captured when the materialize was kicked. The group is re-resolved by NAME (line 359), but `ti` is a raw index: if the user closes a lower-index tab in the same group while the (potentially minutes-long: 'a wedged podman') spec resolution is in flight, `ti` now denotes a DIFFERENT tab. `materialize_with_specs` (panes.rs:706-891) then spawns a pane per spec id (`if self.table.contains_key(old) … continue` doesn't fire — the ids are the closed tab's reserved leaves) and `tab.center.remap(&mut |old| map.get(&old)…)` is a no-op on the mismatched tree, so the freshly-forked shells enter `panes.table` referenced by no tab: invisible, unkillable via UI, leaked until exit (and counted as live panes).
- **Impact:** Leaked live shell processes / PTY fds and possible pane spawn into an unrelated tab after a tab close races a slow sandbox bring-up. Narrow window in practice, but the window scales with provision time (documented as seconds-to-minutes).
- **Fix:** Validate the batch against the tab's content, not its index: carry the missing-leaf ids in the SpecBatch and locate the tab whose `center.pane_ids()` contains them (or drop the batch when no tab matches), mirroring how `handle_exit` finds owners by leaf id.
- **Status:** ⏳

### [session-persist] New-terminal wizard submit spawns the terminal synchronously on the event loop, including sandbox backend probing

- **Where:** `crates/thegn-host/src/run.rs:11434` · effort: small
- **Evidence:** Wizard `Outcome::Submit` calls `spawn_worktree_shell_pane(&mut panes, keymap.config(), cwd…, true, Some(&choice.connection), &choice.sandbox)` inline in the key-dispatch arm, which runs `crate::panes::terminal_launch_spec(cfg, connection, sandbox)` (run.rs:4683-4689). The materialize path deliberately moved this exact call off-thread because 'backend probing (`pick_backend` → `available` subprocess probes) runs synchronously in `terminal_launch_spec` on THIS thread and can stall for seconds on a wedged runtime' (handlers/materialize.rs:155-159). For a local sandboxed terminal (wizard Sandbox pick) those podman/docker/bwrap `available` subprocess probes run on the compositor loop.
- **Impact:** Multi-second UI freeze (no input, no render) on a user-invoked path when the container runtime is slow/wedged — violates the loop's no-blocking-I/O invariant. The lazy path that avoids this already exists and is used by sidebar activation.
- **Fix:** Mirror `sidebar_activate.rs:71-86`: on Submit, push the group with a `panes.reserve_ids(1)` placeholder leaf and let `maybe_materialize` resolve `terminal_launch_spec` off-thread (it already handles the is_terminal path and the splash); delete the synchronous spawn.
- **Status:** ⏳

### [test-gaps] daemon/mod.rs janitors (boot sweep, idle-exit) untested — 421 LOC, 0 tests, both can kill live user sessions if wrong

- **Where:** `crates/thegn-host/src/daemon/mod.rs:162` · effort: small
- **Evidence:** Boot sweep: `for row in db.daemons()... if row.scope == scope && !pid_alive(row.pid) { let _ = db.clear_daemon_leases(...); let _ = db.del_daemon(...) }` (lines 164-168) and `idle_exit_loop` (lines 400-421: `let busy = !svc.sessions.lock().await.is_empty(); ... shutdown.notify_waiters()`) have no tests. Daemon module overall: 2,125 LOC with 10 tests, all confined to session.rs (actor) and service.rs (lease glue); mod.rs + client.rs (686 LOC) have zero.
- **Impact:** The daemon is new and load-bearing (owns every persisted terminal). An over-eager boot sweep (e.g. a scope-comparison or pid_alive regression) deletes a LIVE daemon's registry row + leases → discovery spawns a duplicate daemon and previously-detached sessions become unreachable (user-visible loss of running terminals). An idle_exit regression that miscounts busy (e.g. checks leases instead of sessions) shuts the daemon down while sessions exist — killing users' running processes.
- **Fix:** Extract the sweep decision into a pure fn `fn sweep_targets(rows: &[DaemonRow], scope: &str, alive: impl Fn(i64)->bool) -> Vec<String>` and unit-test it (same-scope-dead ⇒ swept; same-scope-alive and other-scope-dead ⇒ kept). For idle-exit, a tokio test with a nonzero sessions map asserting the shutdown Notify never fires within the window, and an empty map asserting it does.
- **Status:** ⏳

### [test-gaps] control-plane daemon discovery (freshest-heartbeat selection) untested

- **Where:** `crates/thegn-svc/src/control/client.rs:39` · effort: trivial
- **Evidence:** `pub fn discover(store: &dyn ControlStore, scope: &str, now_ms: i64) -> Option<ControlAddr> { let mut live = store.live_daemons(scope, now_ms, DAEMON_HEARTBEAT_TTL_MS).ok()?; live.sort_by_key(|d| d.heartbeat_at); live.pop()... }` — pure, store-driven, zero tests (whole file has none). Mitigated at runtime by the health-probe fallback in `connect_daemon` (daemon/client.rs:45-59).
- **Impact:** A regression (TTL comparison flipped, sort inverted, scope filter dropped) makes every CLI verb and pane spawn take the slow probe/spawn path or, worse with the scope filter dropped, attach to another state-dir's daemon (crossing `just start` isolation). Low likelihood, but the function is trivially testable and currently dark.
- **Fix:** One unit test with an in-memory ControlStore: three rows (stale-heartbeat, fresh-other-scope, fresh-same-scope) ⇒ returns the fresh same-scope endpoint; all-stale ⇒ None.
- **Status:** ⏳

### [test-gaps] run_tests.rs unsets XDG_STATE_HOME instead of restoring it — hermeticity leak under in-process cargo test

- **Where:** `crates/thegn-host/src/run_tests.rs:872` · effort: trivial
- **Evidence:** `unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) }; ... unsafe { std::env::remove_var("XDG_STATE_HOME") };` (pattern repeated at 872, 899, 979, 1038, 1119, 1165+). If the runner exported XDG_STATE_HOME (which it does inside a thegn sandbox / `just start` isolation), the var is permanently REMOVED for the rest of the process, not restored. Contrast agent_tests.rs:412-413 which correctly restores the prior value. Under nextest (per-test processes) this is masked; under `cargo test -p thegn-host` (the CLAUDE.md-recommended precise-test loop) it leaks across tests.
- **Impact:** Order-dependent flakes, and any later test in the binary that calls `Db::open()` without setting its own state home falls back to the user's REAL `~/.local/state/thegn/thegn.db` — a test writing into the live DB of the very thegn instance the dev is working in (the repo's own gotcha list calls this out).
- **Fix:** Replace the manual set/remove pairs with `testenv::EnvGuard::set(&[("XDG_STATE_HOME", ...)])` (already used elsewhere in the crate), which snapshots and restores the prior value on drop, panic-safe.
- **Status:** ⏳

### [test-gaps] pane.rs test mutates GH_TOKEN without ENV_LOCK — races EnvGuard-respecting tests under in-process runs

- **Where:** `crates/thegn-host/src/pane.rs:1322` · effort: trivial
- **Evidence:** `unsafe { std::env::set_var("GH_TOKEN", "leak-me-if-you-can") };` ... `unsafe { std::env::remove_var("GH_TOKEN") };` (line 1339) in `spawn_with_env_firewalls_launcher_creds_but_keeps_infra`, taken WITHOUT `testenv::ENV_LOCK`. The in-code comment argues the set is harmless for children, but the crate's own env-lock contract (testenv.rs: "tests that mutate it must serialize on a single crate-wide lock") is violated: agent_tests and mem.rs tests hold ENV_LOCK while reading/writing env in the same binary. Also: if the test panics between set and remove, GH_TOKEN=leak-me-if-you-can leaks to all subsequent tests.
- **Impact:** Under `cargo test -p thegn-host` (in-process parallel threads), a concurrently running ENV_LOCK test that reads the ambient env (e.g. agent provisioning-env tests around GH_TOKEN — agent.rs:1081 reads it) can observe the poisoned token ⇒ rare, unreproducible flakes blamed on unrelated code. Masked by nextest, so it bites exactly when someone runs the documented precise-test loop.
- **Fix:** Wrap in `let _env = crate::testenv::EnvGuard::set(&[("GH_TOKEN", "leak-me-if-you-can")]);` — also gives panic-safe restore.
- **Status:** ⏳

### [ux-polish] No git (or gh) dependency check anywhere — doctor is silent and launch degrades to an empty UI with no diagnosis

- **Where:** `crates/thegn-host/src/cmd/doctor.rs:206` · effort: small
- **Evidence:** `grep -rn 'have("git")\|which_path("git")\|have("gh")'` over all crates returns nothing. doctor.rs `run()` (lines 206-306) reports terminal caps, sandbox, hosts, provider cache, managed tools, MCP servers, and network — but never probes `git` or `gh`. thegn_core::startup::run_checks (startup.rs:33-51) only repairs masked home paths. Every startup git read is best-effort (`.ok()`/`filter(status.success())`, e.g. run.rs:564-575), so a missing git binary produces no message at all.
- **Impact:** On a machine without git (minimal containers, fresh macOS before xcode-select), a git-worktree IDE launches into a silently empty sidebar/diff panel, and the tool the wizard's Tour step points users at for diagnosis ("thegn doctor — capability + sandbox report", onboarding.rs:1375) has no section that would reveal the root cause. The wizard's gh probe (handlers/onboarding.rs:422-445) handles missing gh well — but only inside the wizard.
- **Fix:** Add a small "Core dependencies" section to `thegn doctor` (text + json): `git` presence/version (`which_path("git")` + `git --version`) and `gh` presence/auth state (reusing `probe_forge`). Optionally emit a one-line startup status when `git` is absent.
- **Status:** ⏳

### [ux-polish] Removed managed-pi / `thegn agent setup` references remain in the seeded example config and shipped-source comments

- **Where:** `config/config.toml.example:694` · effort: trivial
- **Evidence:** config.toml.example:694-695: "kinds dedup (e.g. a managed-pi \"Agent\" + a \"Vanilla Pi\" → one `pi`)" — this file is not just docs: `thegn config edit` seeds it verbatim into the user's config on first edit (cmd/config.rs:9-10 `const EXAMPLE: &str = include_str!(...config.toml.example)`, "seeded on first `config edit`"). Same stale layer also referenced in: thegn-host/src/managed_tool.rs:22 ("wedges `thegn agent setup`, `debug setup`, and (via the sprite `managed_pi`...)"), cmd/mcp.rs:35 ("what agent setup injects"), agent.rs:939/1007 ("the managed pi's `provider = \"pi\"`"), panes.rs:783, thegn-core/src/grants.rs:12 ("First-party tools (the managed pi, ...) are implicitly trusted"), thegn-core/src/managed_tool.rs:102.
- **Impact:** A first-run user's seeded config file documents a concept (managed-pi) that no longer exists, and the mcp.rs doc string is user-visible CLI help (`thegn mcp emit` subcommand description: "what agent setup injects" — a command that no longer exists). Misleading for public-alpha users and contributors; the grants.rs comment also misstates the current trust set.
- **Fix:** Sweep the listed sites: reword config.toml.example:694 to a generic example, fix the `thegn mcp` subcommand doc string (it is clap-visible help text), and update the six comments to name the kept features (bugstalker/managed tools, `[[agents]]` picker) instead of pi/agent-setup.
- **Status:** ⏳

## Prior-audit deferred items (re-verified 2026-08-14)

- **F8-git-init-on-loop** — still-open P3. run.rs:12117 `git init` runs on the loop, but ms-scale + explicit user confirm + documented. Acceptable deferral.
- **F7-db-casts** — still-open P3. 8 `as u32` casts in db_placement.rs, 3 in db.rs; app-controlled writes, near-zero risk. Acceptable.
- **tabbar-env-width** — still-open P3. tabbar_env.rs:43 uses `.chars().count()` not unicode-width — wide glyphs in env names misalign the chip cluster. Minor.
- **keyring-off-loop** — still-open P2. onboarding.rs:387 `keyring_available()` synchronous probe on the launch path. (Also surfaced by ux-polish dimension.)
- **daemon-loop-test** — partially-addressed P3. daemon has 6 tests (session+service) but daemon/mod.rs janitors (boot sweep, idle-exit) have 0. (Surfaced by test-gaps.)
- **machine0-restore-timeout** — gone n/a. No RESTORE_TIMEOUT remains in thegn-svc; the concern no longer applies.
- **db-writer-sweep** — still-open P3. ~57 loop-side `Db::open()` sites remain in run.rs (fire-and-forget best-effort writes). Not per-keystroke hot-path; latency/cleanliness debt.
- **runrs-extractions** — deferred cleanup. run.rs is 17,834 lines (shrunk from ~18,055 by the excision). Pure-move refactor, no correctness benefit; deferred.
- **thin-core-modules** — still-open P3. plugin_api/db_aux/config_vpn/picker/config_ci have 0 unit tests (plugin_api/picker are in the coverage cov_ignore set).
