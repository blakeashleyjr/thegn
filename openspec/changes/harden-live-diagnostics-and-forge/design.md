# Design

Keyboard diagnostics use a closed allowlist of named non-text keys and generic
classes for all other keys. Matching state is logged as forwarded/pending/matched;
formatting an arbitrary action is deliberately avoided. The helper performs no
I/O when its tracing target is disabled.

Network threshold changes preserve the observed FSM, failure count, and last
recovery probe. New thresholds affect subsequent evidence and cadence checks;
changing policy alone does not manufacture a connectivity transition. Atomic
state publication occurs under the FSM lock, while callbacks run after unlock.

The existing native client only implements api.github.com. Unsupported hosts
are rejected before credential lookup, and enterprise ladders contain the CLI
alone. GraphQL error envelopes become octocrab::Error::Graphql and trigger the
existing NotConfigured fallback contract. HTTP error responses establish
reachability but preserve operation failure: authentication and rate-limit
classes stay final; 5xx remains an operation error, not proof of an internet
outage. Transport errors and request deadlines feed the connectivity circuit.

The credential helper receives null stdin/stderr and a capped stdout reader.
Its deadline covers both child exit and stdout EOF, including descendants that
inherit stdout. The leader remains unreaped until stdout completes, anchoring the process group
against numeric-ID reuse; no group signal occurs after leader reaping. Existing
cross-platform process-group helpers provide isolation and group termination.
Cancellation uses the same ownership guard as timeout. Cleanup polls for at most
100 ms, then delegates exceptional delayed reaping; a four-helper permit budget
remains held through delayed reaping, so repeated failures cannot accumulate
unbounded processes or reaper threads. This owns descendants remaining in the
helper's process group, not deliberately detached sessions. A failed reaper-thread handoff retains child ownership and its permit in a
bounded pending queue, retried on the next lookup; wait errors use reaper-only
cleanup without a numeric group signal. No credential values or helper stderr
are logged.

Config warning suppression stores at most 256 fixed-size fingerprints. The key
includes the actual diagnostic, source identity/content, and call site. Nested
source scopes are thread-local and restored on return. Changed content or a new
diagnostic can emit again; eviction can also allow an old diagnostic to recur.
Source revisions invalidate that source's old warnings even when the corrected
file emits none, so fixing and later reintroducing a warning emits again. The
source-revision map is also capped at 256 fingerprints. Strict validation
continues returning complete diagnostics independently of the runtime warning cache. This does not install a timer or background task.

Tests use isolated tracing subprocesses, an in-process tower HTTP service, and
synthetic credential helpers. They never consult actual GitHub accounts or
live configuration/state. User-provided logging directives remain authoritative.

## Plain structured fields (THE-635)

Brand's prefix color flag did not control the outer tracing layer's field writer.
Plain sinks could therefore emit styled field names even with an unstyled prefix.
The formatter must explicitly disable field-writer styling for plain output,
without stripping payload text or changing selected terminal color/JSON policy.
Regression fixtures force an ANSI-capable outer layer so ambient NO_COLOR cannot
silently mask this behavior.
