## Context

Every external door projects `thegn_core::capability::CATALOG`; the HTTP router is already _built from_ the `ROUTES` table. That table is therefore the natural spine for a generic client: verb → route → request, no per-verb client code.

## Decisions

- **`thegn api call` resolves through `ROUTES`,** not a hand-written match: path template + method come from the table, params fill `{placeholders}` then the body/query. New verbs become callable the moment they get a route — the coverage tests already force that moment to exist.
- **MCP state tools stay core-pure**: `StateRouter` owns descriptions, scope gating and routing; the host injects one fetch closure (control client + DB fallback). Mirrors `DocsRouter` exactly.
- **`pr.status` projects the DB cache** (worktree PR rows the panel already shows) rather than hitting the forge: the control plane answers instantly and offline; freshness is the hydration pipeline's job.
- **`notify.push` writes the notification store** through the same path in-process producers use, so routing/DND/sound rules apply to API-pushed notes identically.
- **Scopes**: MCP `--scopes` and `thegn api` both reuse `required_scope` — no new policy vocabulary anywhere.

## Risks / Trade-offs

- The generic `call` exposes exactly what routes expose — including mutating verbs; it authenticates like any control client (scope-checked server-side), so this adds reach, not privilege.
- gRPC mirroring adds proto surface to maintain; the coverage table keeps it honest.
