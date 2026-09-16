# Bounded output ownership

Keep stdout and stderr readers draining for the child lifetime. Limit each
input line to 4096 bytes and discard oversized lines through their newline.
Bound lossy UTF-8 conversion and queued diagnostics; preserve a separate bounded
URL signal so noise cannot displace startup readiness. Retry interrupted reads
and preserve an unterminated final URL at EOF. Recheck the URL signal before
reporting that all diagnostic senders have disconnected.

Redact plan arguments, environment values, URL rules, public addresses and raw
provider error output. Preserve configured authentication and subprocess
environment behavior until their complete security changes can be delivered.
No new process-tree custody or bounded-reaper guarantee is claimed.
