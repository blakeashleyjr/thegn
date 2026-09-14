# Repair session recovery, isolation admission and terminal teardown

Linear: THE-615, THE-616, THE-617, THE-618 (parent THE-614).

## Why

The September 13 live-build audit found missing daemon sessions cannot recover,
transient attach failures abandon otherwise live shells, host fallback skips a
configured isolation floor, and termwiz panics during terminal hangup cleanup.

## What Changes

- Query authoritative session absence through the actual daemon/provider source;
  never interpret transport, authorization or malformed-response failures as absence.
- Retry attachment of the same session within a bounded, cancellable budget;
  preserve pending controls through the existing bounded channel.
- Enforce the floor against the final execution class, including host and remote
  bare execution, and describe unavailable runtime probes without guessing why.
- Patch the pinned termwiz 0.23.3 destructors to finish cleanup without panicking
  when terminal I/O or mode restoration fails.

## Impact

Roadmap: I.111 detach/attach, I.112 reboot resurrection, I.113 layout restoration,
J.121 remote attach, and AB/AC container isolation. Related historical work:
THE-84/THE-85 (restoration), THE-47 (floor), THE-54 (panic restoration).
Exact-session retries overlap THE-265; this change does not implement worktree-
latest fallback or change queue-worker admission owned by THE-577.

No database schema, user configuration keys, keyboard actions or live runtime
changes. Existing pane/exit/fallback events retain their render/waker paths.
