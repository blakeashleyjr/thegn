## Context

Seam traits are now object-safe (`BoxFuture`), so a plugin-backed implementation is just another `Box<dyn IssueBackend>`. The open question was transport and lifecycle: seam calls happen on hydration worker threads; plugin sessions live with the event loop.

## Decisions

- **Correlation lives in svc, lifecycle in host.** `ProviderBridge` knows nothing about the loop: callers block on a channel with the plugin's own timeout; the host's existing `SessionEvent::Response` drain is the one place replies enter, so no new threads and no loop involvement.
- **A process-global registry, not plumbing.** Hydration builds routers from config on its own threads; threading loop-owned state through every call site would touch a dozen signatures. The registry is a published snapshot (same argument as `ci_refresh`'s health map): consumers get the live set at build time, and staleness degrades to a send error, not a wrong answer.
- **Issues first, by selection shape.** `[[issue_accounts]]` is an open list — a plugin account composes with zero config-model change. CI (`[ci] provider`) and forge (`[[forges]] kind`) select by closed `config_enum!` kinds; extending those honestly is its own change, so their extension points ship as wire vocabulary negotiated unsupported rather than pretending.
- **`unsupported` is a first-class plugin answer** mapping onto the seam's optional-op semantics — a plugin implements the ops it has, like any provider.

## Risks / Trade-offs

- One leaked `&'static str` per plugin provider id (seam ids are `&'static`); bounded by the config's plugin count per process life.
- Bridge ids start at 1e6 to stay visually distinct from plugin-allocated `host.call` ids in logs; the two number spaces are independent either way.
