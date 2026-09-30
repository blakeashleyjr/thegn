# Validate sandbox named-volume sources before launch

THE-247 closes a failover hole at the programmatic sandbox boundary. A
configured `SandboxSpec.volumes` source is a named-volume identifier only when
it satisfies Thegn's documented portable lexical policy. Path-like sources and
option syntax must be refused before an OCI argv is built or a caller can
fall back to an uncontained host launch.

The change keeps the policy narrow: it does not add Compose volume syntax,
filesystem lookup, path normalization, network authority, or host fallback.
`None` and disabled sandbox paths retain their existing behavior. Valid names
already accepted by Thegn remain accepted.

The refusal is typed and bounded. It records only the volume pair index, byte
length, and a fixed reason, so callers can surface a useful terminal error
without echoing source text, control bytes, secrets, or an unbounded worktree
path.
