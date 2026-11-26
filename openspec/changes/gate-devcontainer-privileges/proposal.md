# Gate devcontainer privilege requests

## Summary

Devcontainer inputs can request host authority through fields and mounts. Add a
canonical privilege-request model and refuse engine-socket or thegn control and
state directory binds before the generic `devcontainer.mounts` trust approval
can authorize them. The request remains refused until a per-repository grant
path is implemented.

## Impact

- **THE-2144 (WS07-P0-05):** classify protected bind mounts and report a
  fail-closed refusal.
- **WS07-P0-04:** consumes the shared request types and engine-socket paths for
  merged configuration and post-start checks.
- **Affected spec:** `devcontainer`; modifies the unsafe-fields requirement.

## Non-goals

- Adding privilege grants or a `devcontainer_privileges` configuration key.
- Checking merged configuration or created containers; that is WS07-P0-04.
- Changing tmpfs or anonymous-volume behavior.
