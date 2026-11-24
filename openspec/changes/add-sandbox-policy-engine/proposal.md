# Add an operation policy at launch admission

## Summary

thegn's sandbox decides how a process is contained, but nothing yet lets the
user say _which operations_ may start at all. This change adds a **declarative
operation policy**: an ordered list of allow, deny and ask rules in
`[[sandbox.policy]]` (user config only), evaluated by a pure decision function in
`thegn-core` (planned module `op_policy.rs`), in front of the two places where
thegn starts work on a caller's behalf:

- the `tools.run` row of `capability::CATALOG` ("Run one trusted configured tool
  by name in a worktree"), reached by automations, MCP and the control API; and
- launch admission (`sandbox_admission::admit_launch`, planned by WS02-P5-08),
  reached by every launch route.

A rule matches over bounded context only: tool name, argv shape,
worktree-relative paths, profile, placement and launch class. The first matching
rule wins and a fixed set of hard denies is evaluated before any rule. An empty
policy reproduces today's behaviour. The policy narrows what may start; it never
widens a sandbox ceiling.

This is a rewrite of the earlier proposal of the same name, which placed the
policy inside a tool broker that the pre-alpha excision removed. The owner
decided on THE-113 (2026-10-20) to rewrite the change onto live seams rather than
cancel it.

## Impact

- **THE-113** — stays open; this change and its implementation (WS02-P5-12)
  deliver it.
- **THE-791 gate 3 (THE-794)** — configured tools reach the one admission handle,
  and the operation policy sits in front of both `tools.run` and the handle
  (alignment decisions T3 and T10).
- **Programme items** — WS00-P0-04 (this rewrite, W0) and WS02-P5-12 (the
  implementation, W4). `tasks.md` group BB is the roadmap row for the programme;
  group AJ (security / opsec) is the nearest existing group for the capability.
- **Affected spec** — `sandbox`: ADDs requirements under the heading "Operation
  policy at launch admission". The delta MODIFIES no existing requirement, so it
  never blocks the archive of another change that MODIFIES a sandbox requirement
  (the egress changes of WS04).
- **No DB schema change** — policy is read from user config; "always" choices
  append to a trusted user overlay file, not a table.

## Rationale

The sandbox answers "what can this process reach once it runs". The operation
policy answers "may this run at all, and does the user want to be asked first".
Two properties make the common case frictionless and the dangerous case a hard
stop: a read-only tool under the worktree needs no prompt, and a write to git
configuration or to credential material is refused by a fixed list that no rule,
profile or repository overlay can lift. Headless callers (automations, MCP, the
control API) cannot answer a prompt, so they get a typed refusal instead of a
block.

## Non-goals

- **Replacing the sandbox.** The backend, profile and mount model are unchanged;
  the policy is a gate in front of a launch, never a substitute for containment.
- **A syscall or OS profile language.** The policy is over the operations thegn
  itself starts, described by tool name, argv shape and worktree-relative paths.
- **Network policy.** Egress policy is the concern of the egress changes.
- **An AI dependency.** The decision function is pure core and evaluates any
  operation; it is not agent-specific.
- **Implementation.** This change is the contract; WS02-P5-12 implements it.
