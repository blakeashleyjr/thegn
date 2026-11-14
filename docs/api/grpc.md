# gRPC control methods

The `thegn.control.v1.Control` service exposes these batch 113 additions:

| Method            | Capability         | Purpose                                                                |
| ----------------- | ------------------ | ---------------------------------------------------------------------- |
| `AgentSessions`   | `agent.sessions`   | Discover bounded, credential-free coding-agent session metadata.       |
| `SessionsRecord`  | `sessions.record`  | Start, stop or query a bounded asciicast recording for one session.    |
| `WorktreesCreate` | `worktrees.create` | Create a worktree, optionally from a tracker issue or explicit branch. |

The wire definitions are in
[`crates/thegn-svc/proto/thegn/control/v1/control.proto`](../../crates/thegn-svc/proto/thegn/control/v1/control.proto).
