# Add a declarative policy engine over the sandbox

## Summary

superzej's sandbox boundary is currently binary — a process is either sealed in a
container (bouncer: `network=none`, tools brokered over unix-socket ACP with
approval) or it runs on the host. This change adds a **declarative, fine-grained
policy layer** _inside_ that boundary: a `[policy]` config of ordered
allow/deny/ask rules over file paths and shell commands, evaluated by a **pure
decision function** at the bouncer / ACP-approval seam. It turns "one big
approval gate" into graduated control — auto-allow reads under the worktree,
auto-deny writes outside it, _ask_ for anything touching a secret path — without
weakening the container isolation underneath.

## Impact

- **R 232** — ACP permission requests → UI: the policy decides `allow` / `deny` /
  `ask`; only `ask` surfaces the existing permission overlay, and
  `allow_always`/`reject_always` choices persist back as policy rules.
- **AJ (brokerage/opsec) / bouncer** — the sealed-container tool-broker
  (`[llm_proxy].bouncer`) gains a policy check before it services a brokered file
  or shell tool call, so the sandbox boundary becomes rule-driven, not all-or-nothing.
- **AB/sandbox capability** — extends the sandbox behavior with an in-boundary
  policy check; the backend selection and bind-mount model are unchanged.

Extends the `sandbox` capability. **No DB schema change** — policy is derived from
config; persisted `always` choices append to config (or a policy overlay file),
not a new table.

## Rationale

Forge ships a policy rules engine (`forge_domain/src/policies/`) doing path-based
`deny`/`allow`/`require-approval` plus a restricted-shell mode — but Forge has no
real sandbox, so the policy _is_ its only protection. superzej has the inverse:
strong container isolation but a coarse, binary approval gate. The two compose
well — keep the container as the hard floor, add a declarative policy for the
_graduated_ decisions inside it (which reads are free, which writes need a
prompt, which paths are always forbidden). This makes the common case
frictionless (reads under the worktree auto-allow) while keeping a hard stop on
the dangerous case (writes to `~/.ssh`, `.git/config`, secrets), and it feeds the
ACP permission flow that already exists on the roadmap.

## Non-goals

- **Replacing the container/bouncer isolation** — the sandbox stays the hard
  boundary; policy is a _finer_ gate inside it, never a substitute for
  `network=none` / sealing.
- **A general OS sandbox profile language (seccomp/AppArmor authoring)** — policy
  is over the ACP/tool-broker file & shell operations superzej mediates, not
  arbitrary syscalls.
- **Network policy** — egress/tunnel policy is the sandbox-VPN/sealed-tunnel
  feature's concern; this change is file + shell operations only.
- **AI hard-dependency** — the decision function is pure core and evaluates any
  brokered operation; it is not agent-specific.
