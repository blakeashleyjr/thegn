## Context

`destroy_provider_sandbox_with` returns a typed `Teardown`. After the provider's destroy returns OK it polls `inventory()` until a complete listing lacks the name (60 s deadline, 3 s polls). The pool-spare row and provisioned marker are kept until then.

## Decisions

- `wt rm` refuses when no provider is usable, the destroy is unconfirmed, or a provider location sits under a non-provider env. The way out is the token or `env set --worktree <wt> <local env>`.
- `land --release-sandbox` prints `! sandbox <name> kept: <why>` after "landed" and keeps the location; exit code unchanged.
- Sprites credential forwarding is unchanged.
