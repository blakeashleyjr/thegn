# Tasks — shared agent-task engine

## 1. Pure engine (thegn-core)

- [x] 1.1 New `crates/thegn-core/src/agent_task.rs`: `TaskKind`
      (`MergeConflict`/`GateFailure`, extensible), `TaskVars` (ordered var map),
      `TemplateError`. Register in `lib.rs` (alphabetical `pub mod`).
- [x] 1.2 `render_prompt(template, vars)` — `{var}` substitution, `{{`/`}}`
      escapes, **no** shell quoting. Unknown placeholder ⇒
      `TemplateError::UnknownVar` naming the kind's valid variables.
- [x] 1.3 `substitute_command(template, prompt, vars)` — same syntax, but every
      value passes through `util::sh_quote`. Keeps `{prompt}`/`{branch}`/
      `{target}` working exactly as `merge_driver::substitute` does today.
- [x] 1.4 `validate_template(template, kind, is_command)` — unknown-placeholder
      check plus, for command templates, the quoted-placeholder lint (a
      placeholder inside a `'`/`"` run).
- [x] 1.5 `default_prompt(kind)` — the built-in templates, byte-identical in
      rendered form to today's `merge_driver::build_prompt` output.
- [x] 1.6 `headless_command(provider, command)` — the provider→flags table
      (`claude`/`codex`/`aider`, else `<command> {prompt}` + a `config_warn`).
- [x] 1.7 `resolve_agent(cfg, agent, agent_command)` — precedence
      `agent_command` > `agent` (via `[[agents]]`/`[[tools]]` lookup + provider
      inference) > `None`.
- [x] 1.8 Unit tests for 1.2–1.7 (95% line gate on core), including: multi-line
      prompts with quotes/`$`/newlines survive `substitute_command`; unknown and
      quoted placeholders error; every provider-table entry; and the
      **behavior-preservation test** asserting each `default_prompt` renders
      byte-identically to the pre-change `build_prompt` literal.

## 2. Config

- [x] 2.1 `[merge_queue] agent` (String, default `""`) on `MergeQueueConfig` +
      `Default` + the `MergeQueueOverlay` exhaustive destructure.
- [x] 2.2 `[merge_queue.prompts]` sub-table (`conflict`, `gate_failure`; empty ⇒
      built-in) + overlay support, so a repo can carry its own prompts via
      `[workspace.<slug>.merge_queue]`.
- [x] 2.3 Wire `validate_template` into `config_validate` so
      `thegn config validate` reports a bad template.
- [x] 2.4 Document every new key in `config/config.toml.example`
      (`tests/config_example.rs` fails otherwise).
- [x] 2.5 **Fix the existing example defect**: `agent_command`'s sample shows
      `-p "{prompt}"`; placeholders are already shell-quoted, so drop the quotes
      and note the bare-placeholder contract inline.

## 3. Host runner (thegn-host)

- [x] 3.1 New `crates/thegn-host/src/agent_run.rs`: lift `merge_driver::run_agent`
      verbatim — `spawn_grouped` process group, timeout watchdog, 1 MiB-capped
      stdout/stderr drains, `util::GIT_ENV_VARS` scrub, login shell, cwd =
      worktree — generalized over `TaskKind` + `TaskVars`.
- [x] 3.2 Env contract: `THEGN_TASK_KIND`, `THEGN_TASK_PROMPT`, `THEGN_WORKTREE`,
      `THEGN_BRANCH`; retain `THEGN_MERGE_PROMPT`/`THEGN_MERGE_TARGET` as
      documented-deprecated aliases for the two merge kinds.
- [x] 3.3 Keep the single `#[cfg(not(unix))]` stub here, and compose **argv**
      rather than a shell string where the login-shell wrapper does not require
      one, so the Windows port is a quoting change rather than a rewrite.
- [x] 3.4 Gate the spawn-only imports (`Arc`/atomics/`Duration`) on `unix` — the
      stub needs none of them, and `check-cross` fails on the unused imports.
- [x] 3.5 Leave `merge_driver::run_agent`/`compose` **un**gated: `agent_run::run`
      carries the Windows stub, so a `#[cfg(any(unix, test))]` here left the
      dispatch arm calling a function absent on the Windows target
      (caught by `just check-cross`, not by `just test`).

## 4. Merge-queue retrofit

- [x] 4.1 Delete `build_prompt`, `substitute`, `run_agent` from
      `merge_driver.rs`; call the shared engine. `drive_queue`'s decisions,
      statuses, attempt budget, and exit-code handling are unchanged.
- [x] 4.2 Resolve the agent once per drain (not per branch) and degrade to
      notify when resolution yields nothing — today's empty-`agent_command`
      behavior.
- [x] 4.3 Surface a resolution warning through `tracing`/`model.status` rather
      than silently not dispatching.

## 5. Tests

- [x] 5.1 The existing real-git driver tests
      (`agent_resolves_conflict_and_branch_lands`,
      `agent_that_cannot_fix_marks_needs_human`) must pass **unmodified** — the
      regression gate for this refactor.
- [x] 5.2 A drive test using `agent = "<name>"` resolution instead of
      `agent_command`, and one using a custom `[merge_queue.prompts] conflict`
      template (assert the fake agent observes the custom text via
      `THEGN_TASK_PROMPT`).
- [x] 5.3 `config_tests`: the new keys' defaults, overlay round-trip, and
      `config validate` rejecting a bad template.

## 6. Docs + validate

- [x] 6.1 Update `docs/help/merge-queue.md` for the new `agent` / `prompts` keys.
      (No new action ids, so `test/help-ratchet.txt` is untouched.) While there,
      correct its stale `thegn mq add` references — the CLI verb is
      `thegn merge add`.
- [x] 6.2 Mark **T 758** in `tasks.md` with the configurable-prompt/agent work.
- [x] 6.3 `git add` the two new modules before `nix-build` — the flake source
      allowlist only sees git-tracked files, so an untracked `agent_task.rs`
      fails the sandboxed build with `E0583: file not found for module` while
      every local gate is green.
- [x] 6.4 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
