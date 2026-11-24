# Tasks

Implemented by WS02-P5-12 (W4); nothing below is started.

## 1. Core policy (thegn-core)

- [ ] 1.1 `op_policy.rs`: `OpContext`, `OpDecision`, ordered first-match
      evaluation over tool name, argv shape, worktree-relative paths, profile,
      placement and `LaunchClass` — **unit tests**: each selector, first match
      wins, default fall-through, empty policy is a no-op.
- [ ] 1.2 Hard denies shared with the sandbox deny list — `SEALED_HOME_DENY`
      and `SEALED_ABS_DENY` in `sandbox_mounts.rs` plus the git-config entries
      of `SEALED_HOME_ALLOW` and the repository git-config pins (shared
      `<common>/config`, `[sandbox.git]` `ro_after`), exposed `pub(crate)`
      rather than copied —
      evaluated first — **unit tests**: a user allow rule cannot lift a hard
      deny.
- [ ] 1.3 `[[sandbox.policy]]` rules in user config; the rule action enum via
      `config_enum!`; a repository overlay may add `deny` only — **unit tests**:
      `repo_overlay_cannot_allow`, `hard_deny_survives_repo_overlay`.

## 2. Decision points (thegn-host)

- [ ] 2.1 Evaluate the policy in the `tools.run` executor before spawn —
      **test**: `denied_tool_run_refused_before_spawn` (names the rule).
- [ ] 2.2 Evaluate the policy before `sandbox_admission::admit_launch` mints a
      handle — **test**: `denied_launch_refused_before_spawn`.
- [ ] 2.3 `ask`: TUI status prompt for `InteractiveOwner`; typed refusal
      `policy-ask-headless` for headless callers — **tests**:
      `ask_headless_is_typed_refusal`, `empty_policy_is_noop`.
- [ ] 2.4 Persist always-choices to the trusted user overlay; ignore a choice
      that would widen an outer ceiling.

## 3. Docs, ratchets and validation

- [ ] 3.1 Document `[[sandbox.policy]]` in `config/config.toml.example` and the
      "Operation policy" section of `docs/help/sandbox-backends.md`.
- [ ] 3.2 Ratchets: config-reference and help, `config_example`, env-overlay pin
      (table, file-only), `hm_module_drift` (`OMITTED`),
      `marked_definition_count_is_pinned` (rule action enum), repo clamp
      (accumulate, deny only).
- [ ] 3.3 Run `just ci` (includes `openspec validate --all --strict`) once, when
      the implementation is complete.
