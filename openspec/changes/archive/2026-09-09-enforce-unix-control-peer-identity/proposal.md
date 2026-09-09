# Enforce Unix control-socket peer identity

Linear: THE-100
Parent: THE-41

## Problem

The local router grants implicit Admin from a listener-level `local_admin`
flag. Unix comments describe peers as same-UID, but accepted streams are not
authenticated with native peer credentials. Owner-only directory/socket modes
mitigate access but best-effort chmod is not per-connection identity proof.

## Proposed change

- Capture native Unix peer credentials at accept and carry authenticated peer
  metadata through HTTP/gRPC authorization context.
- Grant implicit local Admin only when the peer effective UID equals the
  daemon's effective UID.
- Fail closed when credentials are unavailable or mismatched: require an
  ordinary scoped token or reject, never silently upgrade.
- When implicit local Admin is enabled, treat insecure run-directory/socket
  ownership or permission setup as a surfaced startup/doctor failure.
- Retain a documented token-required mode for platforms/filesystems lacking a
  reliable peer credential API.

## Baseline retained

The daemon attempts run-dir 0700/socket 0600; relocated sockets require an
owner-controlled private temporary directory; TCP disables local Admin; Windows
named pipes already reject remote clients and are outside this Unix-specific
change.

## Non-goals

- Redesigning TCP bearer auth, enabling cross-user local sharing, or changing
  Windows pipe ACLs without evidence of an equivalent gap.
