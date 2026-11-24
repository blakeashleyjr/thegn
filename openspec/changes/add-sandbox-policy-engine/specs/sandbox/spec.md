# Sandbox

## ADDED Requirements

### Requirement: Operation policy at launch admission

thegn SHALL decide whether a `tools.run` call or a launch may start by evaluating
an ordered operation policy, in `thegn-core`, before any process is created. The
two decision points SHALL be the `tools.run` row of `capability::CATALOG`,
reached by automations, MCP and the control API, and the launch-admission handle
(`sandbox_admission::admit_launch`, planned by WS02-P5-08), reached by every
launch route. The rule table SHALL be `[[sandbox.policy]]` in user config only.
The policy yields allow, deny or ask. A rule MATCHES when every selector it
carries matches; the selectors are bounded to the tool name, the argv shape,
worktree-relative paths, the resolved profile, the placement and the launch
class. The first matching rule wins, and when no rule matches the configured
default applies. The decision function MUST be pure and MUST name the rule that
produced a deny or an ask. An empty policy MUST reproduce today's behaviour. The
policy MAY only narrow what starts; it MUST NOT widen a sandbox ceiling.

#### Scenario: A denied tool run is refused before any process exists

- **WHEN** a rule denies a configured tool and an automation, an MCP client or
  the control API calls `tools.run` for it
- **THEN** the call is refused naming the rule, and no process was created

#### Scenario: A denied pane launch is refused before any process exists

- **WHEN** a rule denies a launch and a launch route asks launch admission for it
- **THEN** admission refuses naming the rule, and no spawn helper is reached

#### Scenario: First matching rule wins

- **WHEN** two rules match an operation
- **THEN** the earlier rule's action is applied

#### Scenario: An empty policy is a no-op

- **WHEN** no policy rules are configured
- **THEN** every `tools.run` call and every launch is decided exactly as it was
  before the policy existed

### Requirement: Operation policy hard denies cannot be lifted

The operation policy SHALL evaluate its hard denies before any rule. The hard
denies are writes to git configuration, SSH and GPG material, and the canonical
secret paths, and they MUST be the same list the sandbox enforces rather than a
copy (`SEALED_HOME_DENY`, `SEALED_ABS_DENY` and the read-only git-config entries
of `SEALED_HOME_ALLOW` in `sandbox_mounts.rs`, plus the repository's shared git
config and `[sandbox.git]` `ro_after` paths the sandbox pins read-only). A user allow rule, a profile, a
repository overlay and a persisted always-choice MUST NOT lift a hard deny.

#### Scenario: A repository overlay cannot lift a hard deny

- **WHEN** a repository overlay or a user allow rule would permit an operation
  that a hard deny covers
- **THEN** the operation is still denied by the hard deny

### Requirement: Repository overlays can only deny

A repository overlay of the operation policy SHALL add `deny` rules only. It MUST
NOT add an allow rule and MUST NOT turn a deny into an ask, because the
repository is the least-trusted author of the policy.

#### Scenario: A repository overlay cannot allow

- **WHEN** a repository overlay contains an allow rule
- **THEN** the rule is dropped, the denial is surfaced, and the effective policy
  is unchanged

### Requirement: Ask decisions never block a headless caller

An `ask` decision SHALL resolve through the TUI status prompt when the launch is
an interactive owner launch. For a headless caller (an automation, an MCP client,
the control API or a queue or CI launch) an `ask` MUST be a typed refusal with the
id `policy-ask-headless`, and MUST NOT block waiting for an answer.

#### Scenario: Headless ask is a typed refusal

- **WHEN** a rule resolves to ask and the caller is an automation or an MCP client
- **THEN** the call returns the refusal `policy-ask-headless` immediately

#### Scenario: Interactive ask prompts the user

- **WHEN** a rule resolves to ask and the launch is an interactive owner launch
- **THEN** the status prompt is shown and the operation proceeds only on approval

### Requirement: Always-choices persist only to a trusted overlay

An allow-always or deny-always answer to an ask SHALL persist as a rule in a
trusted user overlay, and MUST NOT widen an outer ceiling: a persisted allow that
would exceed the launch class default or the sandbox floor MUST be ignored for
that launch.

#### Scenario: Always-choice is remembered

- **WHEN** the user answers an ask with allow-always and the allow stays inside
  the outer ceiling
- **THEN** a matching allow rule is persisted and the next identical operation is
  allowed without a prompt

#### Scenario: An always-allow cannot widen a ceiling

- **WHEN** a persisted allow rule would permit a launch that the launch class
  default forbids
- **THEN** the launch is still refused
