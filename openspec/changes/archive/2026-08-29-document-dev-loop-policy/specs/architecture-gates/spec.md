# architecture-gates

## ADDED Requirements

### Requirement: Agent sessions are steered off full-workspace gates while iterating

The repository's AI-harness configuration SHALL register a `PreToolUse`
command guard (`test/heavy-guard.sh`, wired in `.claude/settings.json`) that
refuses the full-workspace invocations it recognizes: the direct heavy `just`
recipes (`test`, `test-doc`, `ci`, `ci-local`, `coverage`, `coverage-html`,
`lint`, `bench`, `bench-micro`, `e2e`, `doc-check`), `cargo llvm-cov`, and
workspace-wide cargo build/check/clippy/test/nextest runs. The guard also
recognizes these forms after the command boundaries implemented by the script,
including `--command`, `exec`, `time`, `nice`, and the supported shell `-c`
runner forms. A refusal SHALL name the scoped alternatives
(`just quick <crate>`, `cargo nextest run -p <crate> <substring>`,
`cargo check -p <crate>`). This is a harness gate, not a `just lint`/`just
test` gate: it steers iteration; the git pre-push hook remains the correctness
gate and runs outside it.

The guard MUST pass a command through unchanged when `THEGN_ALLOW_HEAVY=1`
appears on it, MUST fail open when it cannot parse its input or its
dependencies are missing, and MUST NOT fire on gate names that appear only
inside quoted strings or heredoc bodies. Actual supported shell `-c` runner
invocations remain recognized even though their command text is quoted.

#### Scenario: An iterating agent is redirected

- **WHEN** an agent session runs `nix develop --command just test` mid-iteration
- **THEN** the call is blocked and the refusal names `just quick <crate>` and
  scoped `cargo nextest` as the alternatives

#### Scenario: The deliberate pre-push run passes

- **WHEN** an agent runs `THEGN_ALLOW_HEAVY=1 just test`
- **THEN** the command runs unguarded

#### Scenario: Naming a gate is not running it

- **WHEN** an agent runs `git commit -m "run just ci before pushing"` or
  `grep -r "just test" docs/`
- **THEN** the guard does not block the command
