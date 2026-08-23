## 1. Derive containment from the argv

- [x] 1.1 Add `crates/thegn-core/src/sandbox_truth.rs`: `observed(argv) -> Backend` over command
      words only (argv[0], pass-throughs, post-`--` handoff, first word of each embedded script
      command), so an argument named after a runtime cannot imply containment
- [x] 1.2 Add `reconcile(requested, argv) -> Truth { label, degraded, warning }`, with `""`/`auto`/
      `host`/`none` treated as "wherever the chain lands" rather than a degradation
- [x] 1.3 Document and handle the native-Windows exception (containment invisible to argv)
- [x] 1.4 Register the module in `lib.rs`

## 2. The gate

- [x] 2.1 `every_backend_round_trips`: render the real `enter_argv` per backend and assert the
      derived backend matches
- [x] 2.2 Make the backend list exhaustive by construction (dead `match` over every variant) so a
      new backend fails to compile
- [x] 2.3 Pin the dangerous direction: a path/remote/image named `docker` or `podman` must read as
      `host`
- [x] 2.4 Pin rootless vs rootful podman, the post-`--` transport handoff, the ssh remote-script
      case, and the `host`/`auto` non-degradation cases

## 3. Wire it into the launch paths

- [x] 3.1 `panes.rs::terminal_launch_spec`: label from the argv; on degradation fall through to a
      plainly labelled host shell carrying the warning (the reported bug)
- [x] 3.2 `agent.rs::compose_spec`: reconcile the resolver's label against the argv for local
      placements; keep the resolver's label for remote placements
- [x] 3.3 Surface the warning via `msg::warn` as well as `LaunchSpec.warnings`
- [x] 3.4 Verify: `cargo test -p thegn-core --lib sandbox_truth` and the host tests covering
      `compose_spec`, `terminal_launch_spec`, and `explicit_unavailable_sandbox_does_not_fall_back_to_host`

## 4. Separate recorded intent from displayed containment

- [x] 4.1 Add an observed-containment column for terminals and worktrees. No `user_version` bump:
      the repo's convention for additive columns is a bare `ALTER TABLE … ADD COLUMN` (no-op once
      present, branch-merge-safe), which `db_migrate.rs` already uses for `sandbox_backend` itself
- [x] 4.2 Record the observed value at spawn — `agent::launch_spec_full` for worktrees,
      `handlers::terminal::record_observed` from the materialize path for terminals; keep
      `sandbox_backend` as the intent/override store
- [x] 4.3 Point every display surface at the observed value: `hydrate_terminal::terminal_env`,
      `hydrate::active_backend` (which also stops predicting from config before the first launch),
      and both sidebar row builders (`hydrate.rs` worktrees, `sidebar.rs` terminals)
- [x] 4.4 Tests: `db_tests::intent_and_observed_containment_are_separate_columns` (core, the
      coverage-gated crate) plus `hydrate_terminal::tests::a_pick_that_degraded_to_the_host_never_shows_as_contained`
      and `a_never_launched_terminal_claims_nothing`

## 5. Offer a dormant runtime instead of skipping it

- [x] 5.1 Detect `BackendState::NotRunning` at launch (not just in onboarding) when the chain would
      degrade — `prepare_sandbox_env` builds a `support_report` at the host-fallback seam
- [x] 5.2 Prompt: start / host anyway / cancel, via `menu::sandbox_dormant_menu` + the new
      `MenuChoice::SandboxStartRuntime`; policy is `[sandbox] on_dormant = ask|start|host|cancel`
      (`config_placement::OnDormant`, documented in config.toml.example)
- [x] 5.3 Run the start command off the event loop (`sandbox_start::run`, 90s bounded), then
      `clear_probe_cache()` so the retry re-probes instead of replaying the cached "absent"
- [x] 5.4 Fix the macOS Docker remedy string — `remedy_for` is now OS-aware (colima on macOS,
      Docker Desktop on Windows, systemd elsewhere)
- [x] 5.5 Tests: 10 in `sandbox_dormant_tests` (start-command mapping per OS, first-dormant
      selection, every policy branch, `start`→`ask` downgrade when nothing can be run unattended,
      no prompt for an uncontained launch) plus a host test that the modal offers a start row only
      when there is a command and shows it verbatim

## 6. Pre-PR gate

- [x] 6.1 Gates green: 4300+ tests, clippy, smoke, and `just coverage` (core ≥95% lines, run via
      the nix dev shell — `llvm-tools-preview` is missing from a bare shell)
