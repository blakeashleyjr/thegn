# Agent backends

- `[[agents.backends]]` entries SHALL remain ordered, with unique names and a
  launchable harness.
- `thegn agent run --agent <role>` SHALL select the first configured backend
  with known usage headroom when the role has backends.
- `thegn agent run --backend <name>` SHALL launch that named backend for the
  selected role, even when usage is unknown or exhausted.
- Backend launch SHALL preserve the ordinary sandbox, worktree identity and
  harness credential-home behavior; it SHALL NOT copy credentials between
  accounts.
- Roles without backends SHALL retain the existing single-backend behavior.
- Pipeline stage launches SHALL keep their configured backend until pipeline
  failover attribution is implemented.
