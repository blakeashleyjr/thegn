# Design

## Two decision points

1. **`tools.run`.** The catalog row `tools.run` (`capability::CATALOG`) is the one
   door through which automations, MCP and the control API start a configured
   tool. The policy is evaluated in the executor behind that row, before any
   process is created.
2. **Launch admission.** `sandbox_admission::admit_launch` (planned by WS02-P5-08;
   today's type is `Admission` in `thegn-host/src/sandbox_admission.rs`) is the
   handle every launch route passes through. The policy is evaluated before the
   handle is minted, so a refused launch never reaches a spawn helper.

The admission handle, the launch classes and the refusal table are specified by `add-launch-admission`;
this change only adds a rule evaluation in front of them.

Both points call the same pure function, so a tool started through `tools.run`
and a pane launched by a route are judged by one rule set.

## The decision (pure, core)

`thegn-core` gains a substrate-free module (planned `op_policy.rs`):

- `OpContext` carries only bounded facts: the tool name, the argv shape (program
  and argument count and flags, not secret values), worktree-relative paths, the
  resolved profile, the placement, and the `LaunchClass` (alignment T1).
- `OpDecision` is `Allow`, `Deny { rule }` or `Ask { rule }`; a decision always
  names the rule that produced it so a refusal can say why.
- Evaluation: hard denies first; then the rules in order, first match wins; then
  the configured default. A rule matches when every selector it carries matches.

`LaunchClass` already exists in `thegn_core::launch_class`. The new module sits
beside it, and like that module and `thegn_core::admission` it keeps a name
outside the frozen `sandbox*.rs` glob (B7).

The function does no I/O. It is unit-tested to the core coverage gate.

## Hard denies

The hard denies are writes to git configuration, SSH and GPG material, and the
canonical secret paths. They are **the same lists the sandbox already enforces**,
shared by reference rather than copied so the two cannot drift:

- `SEALED_HOME_DENY` and `SEALED_ABS_DENY` in
  `crates/thegn-core/src/sandbox_mounts.rs` (SSH, GPG, cloud and credential
  material, and the absolute secret roots); and
- the git-config entries of `SEALED_HOME_ALLOW` (`~/.gitconfig` and
  `~/.config/git/config`), which `sealed_home_allowlist` binds read-only, so a
  git config write is already impossible in a sealed home.

Those constants are private today. WS02-P5-12 exposes them `pub(crate)` (or
behind a `pub(crate)` accessor) instead of copying the entries, so a new
credential path cannot be added to one list and forgotten in the other. A user
allow rule, a profile and a repository overlay cannot lift a hard deny.

## Rules and trust

- The rule table (planned `[[sandbox.policy]]`) is user config only.
- A repository overlay (`.thegn.*`) may add `deny` rules and nothing else. It can
  never allow, and it cannot turn a deny into an ask.
- "Always allow" and "always deny" choices persist to a trusted user overlay file.
  A persisted choice cannot widen an outer ceiling (the class default or the
  sandbox floor); it is ignored where it would.
- An empty policy, and an absent file, mean today's behaviour: every launch and
  every `tools.run` call is decided exactly as before.

## `ask` by caller

- `InteractiveOwner` launches: `ask` resolves through the TUI status prompt.
- Headless callers (automations, MCP, the control API, queue and CI launches):
  `ask` is a typed refusal with the planned id `policy-ask-headless`. It never
  blocks, because there is nobody to answer.

## Invariants

- **Event loop.** `decide` is pure and cheap. It runs where the call is made
  (the `tools.run` executor, the launch path), never on the render loop, and adds
  no timer, thread or wake source. The status prompt for an interactive `ask`
  uses the existing status path (channel plus waker).
- **Render.** No new chrome beyond the existing status prompt; render-plan
  invariants are unchanged.
- **State.** No schema change and no `user_version` bump.
- **Fail closed.** A refusal happens before any process exists. A policy that
  cannot be read refuses the operation it would have governed rather than
  allowing it.

## Naming caveat

`LaunchAdmission` and `admit_launch` are created by WS02-P5-08 (W3). Until then
the live type is `Admission`; this design describes `admit_launch` as the planned
seam, not as current behaviour.

## Alternatives considered

- **Bake the policy into backend profiles.** Too coarse and per backend; the two
  decision points above are where thegn already mediates every start uniformly.
- **Prompt on everything.** High friction; the point is to auto-allow the safe
  common case and reserve prompts for risky operations.
- **Let repositories allow operations.** Rejected: a repository is the least
  trusted author, so it may only tighten.
