# Bound public-share output and redact diagnostics

This delivers the existing THE-325 output and diagnostic subtask. No new issue,
provider, or configuration is introduced. A noisy tunnel client currently can
retain an unlimited output line or queue, and debug/error formatting can expose
credential-bearing arguments, environment values, public tickets, or output.

Bound retained reader state, preserve URL discovery and post-start draining,
and redact those diagnostic surfaces. Credential transport, environment
admission, state-file custody, sidecar identity, and process-tree settlement
remain unfinished under their existing issues.
