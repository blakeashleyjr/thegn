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
| containers.control    | grpc    | deferred    | THE-204 | container lifecycle and logs are local CLI-only; gRPC mirror lands in the client-API phase         |
| containers.list       | grpc    | deferred    | THE-204 | container listing is local CLI-only; gRPC mirror lands in the client-API phase                     |
| containers.prune      | grpc    | deferred    | THE-204 | prune is `thegn sandbox gc/prune` (CLI) this change; a control route lands in the client-API phase |
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
| secret.audit          | grpc    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.list           | grpc    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.migrate        | grpc    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.rm             | grpc    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.set            | grpc    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.ssh.rotate     | grpc    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| containers.control    | http    | deferred    | THE-201 | container lifecycle and logs are local CLI-only; HTTP mirror lands in the client-API phase         |
| containers.list       | http    | deferred    | THE-201 | container listing is local CLI-only; HTTP mirror lands in the client-API phase                     |
| containers.prune      | http    | deferred    | THE-201 | prune is `thegn sandbox gc/prune` (CLI) this change; a control route lands in the client-API phase |
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
| secret.audit          | http    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.list           | http    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.migrate        | http    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.rm             | http    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.set            | http    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| secret.ssh.rotate     | http    | unsafe      | -       | control-API route deferred; CLI-only for now                                                       |
| calendar.ingest       | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| containers.control    | mcp     | deferred    | THE-203 | container doors land in the MCP scope-gating phase                                                 |
| containers.list       | mcp     | deferred    | THE-203 | container doors land in the MCP scope-gating phase                                                 |
| dispatches.put        | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| dispatches.set_status | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| git.commit            | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| git.stage             | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| issues.comment        | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| issues.update         | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| merge.add             | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| merge.clear           | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| notify.push           | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| sessions.detach       | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| sessions.resize       | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| sessions.snapshot     | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| sessions.split        | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| worktrees.create      | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
| worktrees.open        | mcp     | deferred    | THE-203 | MCP state tools land in the client-API phase                                                       |
