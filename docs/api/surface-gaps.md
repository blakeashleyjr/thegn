# Control-surface gaps

Generated from `thegn_core::capability::SURFACE_GAPS` and `GAP_DISPOSITIONS` by
`just ratchet-update`; do not edit by hand. `thegn api coverage` reports the same
dispositions.

Disposition codes: `deferred` (owed; the review issue owns the decision), `blocked`
(waiting on the named issue), `superseded` (replaced by the named capability),
`local-only` (operates on caller-local files or custody), `unsafe` (would widen
trust on this surface; not planned).

| Capability            | Surface | Disposition | Review  | Reason                                                                                             |
| --------------------- | ------- | ----------- | ------- | -------------------------------------------------------------------------------------------------- |
| search.query          | cli     | local-only  | -       | local worktree-fs verb; runs in-process, not through the control API                               |
| search.replace        | cli     | local-only  | -       | local worktree-fs verb; runs in-process, not through the control API                               |
| agent.sessions        | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| containers.control    | grpc    | deferred    | THE-204 | container lifecycle and logs are local CLI-only; gRPC mirror lands in the client-API phase         |
| containers.list       | grpc    | deferred    | THE-204 | container listing is local CLI-only; gRPC mirror lands in the client-API phase                     |
| containers.prune      | grpc    | deferred    | THE-204 | prune is `thegn sandbox gc/prune` (CLI) this change; a control route lands in the client-API phase |
| dispatches.list       | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| dispatches.put        | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| dispatches.set_status | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| doctor.bundle         | grpc    | local-only  | -       | local CLI operator verb; no control route (bundle reads local files)                               |
| issues.comment        | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| issues.get            | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| issues.list           | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| issues.update         | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| launch.preset         | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| mcp_proxy.reload      | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| mcp_proxy.status      | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| model_proxy.start     | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| model_proxy.stats     | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| model_proxy.status    | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| model_proxy.stop      | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| program.assign        | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| program.create        | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| program.list          | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| program.new_feature   | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| program.rename        | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| program.rm            | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| project.assign        | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| project.create        | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| project.list          | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| project.new_feature   | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| project.rename        | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| project.rm            | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| secret.audit          | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| secret.list           | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| secret.migrate        | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| secret.rm             | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| secret.set            | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| secret.ssh.rotate     | grpc    | deferred    | THE-204 | control-API route deferred; CLI-only for now                                                       |
| sessions.record       | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| worktrees.create      | grpc    | deferred    | THE-204 | not yet mirrored in control.proto                                                                  |
| containers.control    | http    | deferred    | THE-201 | container lifecycle and logs are local CLI-only; HTTP mirror lands in the client-API phase         |
| containers.list       | http    | deferred    | THE-201 | container listing is local CLI-only; HTTP mirror lands in the client-API phase                     |
| containers.prune      | http    | deferred    | THE-201 | prune is `thegn sandbox gc/prune` (CLI) this change; a control route lands in the client-API phase |
| doctor.bundle         | http    | local-only  | -       | local CLI operator verb; no control route (bundle reads local files)                               |
| launch.preset         | http    | deferred    | THE-201 | CLI-first (`open --preset` via the intents mailbox); an HTTP route is a follow-up                  |
| model_proxy.start     | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| model_proxy.stats     | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| model_proxy.status    | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| model_proxy.stop      | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| program.assign        | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| program.create        | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| program.list          | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| program.new_feature   | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| program.rename        | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| program.rm            | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| project.assign        | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| project.create        | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| project.list          | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| project.new_feature   | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| project.rename        | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| project.rm            | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| secret.audit          | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| secret.list           | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| secret.migrate        | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| secret.rm             | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| secret.set            | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| secret.ssh.rotate     | http    | deferred    | THE-201 | control-API route deferred; CLI-only for now                                                       |
| calendar.clocks       | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| calendar.events       | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| calendar.ingest       | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| containers.control    | mcp     | deferred    | THE-203 | container doors land in the MCP scope-gating phase                                                 |
| containers.list       | mcp     | deferred    | THE-203 | container doors land in the MCP scope-gating phase                                                 |
| dispatches.list       | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| dispatches.put        | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| dispatches.set_status | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| git.commit            | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| git.stage             | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| git.status            | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| issues.comment        | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| issues.get            | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| issues.list           | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| issues.update         | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| launch.preset         | mcp     | deferred    | THE-203 | MCP exec-scoped tools land in the MCP write-tools phase                                            |
| merge.add             | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| merge.clear           | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| merge.list            | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| notify.push           | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| pr.status             | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| sessions.detach       | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| sessions.resize       | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| sessions.snapshot     | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| sessions.split        | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| worktrees.create      | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| worktrees.open        | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
