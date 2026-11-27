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
- A waited `thegn agent run` SHALL retry once on the next ordered backend with
  known headroom when the first run exits after a recognized Claude, Codex or
  Pi quota response. A pinned backend SHALL not fail over.
- Quota classification SHALL be harness-specific and SHALL ignore generic
  connection, authentication, and tool errors.
- A quota response SHALL mark its backend unavailable until the matched
  provider usage window resets, including across later launches.
- Pipeline stage launches SHALL keep their configured backend until pipeline
  failover attribution is implemented.
