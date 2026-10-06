# THE-313 plan

Scope: client side only (process-tree kill is THE-311).

- BridgeClient::new/build return typed BridgeError (reader-thread failure reaps owned child); propagate in bridge_sup native + spawn.
- close(deadline) idempotent: fail pending/subs, close writer, kill+bounded reap child, bounded reader join; CloseReport.
- Drop non-blocking: fail_all, kill, try_wait, bounded reaper thread fallback.
- Tests in bridge/mod.rs: spawn failure, close bounded/idempotent, blocked reader, pending failure, drop timing.
  Out of scope: process-tree termination (THE-311), host lifecycle calling close.
