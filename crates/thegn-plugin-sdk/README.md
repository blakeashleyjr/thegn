# Native plugin SDK

A terminal-free library for trusted native plugin processes. The host remains
responsible for permissions and process supervision; native plugins retain
normal OS access.

`FrameReader` and `FrameWriter` exchange object-valued NDJSON with a complete
encoded limit of 1 MiB, including the newline. The writer checks JSON escaping
before emitting any bytes. Malformed, unterminated, or oversized input ends the
session. Reserve stdout for these frames and write diagnostics to stderr.
Both directions reject duplicate object members, more than 64 nested JSON
containers, mixed message/response envelopes, and non-`u64` request IDs. A response
contains exactly one `result` or `error` arm and its originating request ID.

```rust,no_run
use thegn_plugin_sdk::{FrameReader, FrameWriter};
let mut input = FrameReader::new(std::io::stdin().lock());
let mut output = FrameWriter::new(std::io::stdout().lock());
while let Some(frame) = input.read()? {
    // Decode lifecycle/RPC messages, then dispatch application work.
    // A reply must follow completed local acceptance, not just queue admission.
    drop(frame);
}
# Ok::<(), std::io::Error>(())
```

Use `SessionContext::activate` with the host's activation parameters and validate
all typed app events against that session. `handle_app_request` validates the
callback envelope and gives the handler its ordinary RPC request ID. Reliable
actions require an ID; raw input and lifecycle notifications do not receive a
synthetic acknowledgment. `SessionContext::receipt_key` combines the originating
session and request ID for durable deduplication.

Commit application commands, the durable receipt, and the corresponding draft
transition in one local transaction before returning success. Return
`AppEventResult.field_errors` to reject a form while preserving its values. A
lost response leaves an uncertain outcome: never automatically replay a
mutation in another session. `Cancellation` shares explicit cancellation with
worker threads; it does not forcibly interrupt blocking OS calls.

`PrivateState::from_host` uses `THEGN_PLUGIN_STATE_DIR`. Applications preserving
an older explicit state override can pass an absolute path to `PrivateState::new`.
Read/write operations accept a single filename and an explicit byte limit, are
rooted at an already-opened private directory handle, reject leaf symlinks
without following them, and replace files atomically using the shared
cross-platform filesystem helpers. Keep
`PrivateState::lease("plugin.lock")` alive for the process lifetime when state
requires one writer. Lock files remain in place so another process cannot lock
a different inode while the first lease is held.

`CredentialStore` and `Browser` are injectable interfaces. `OsCredentialStore`
accepts the plugin's stable service namespace and an account key per operation.
It has no plaintext fallback. `OsBrowser` opens an HTTPS URL without embedded
credentials, directly through the OS opener without a shell, and reaps the
opener child on a short, named helper thread. These helpers do not grant access
to host `secret.*` APIs.

`RequestTracker` correlates plugin-originated requests separately from incoming
mutation receipts. It holds at most 32 pending IDs, never reuses/wraps IDs, and
accepts caller-supplied monotonic deadlines. Cancel the reservation if frame
admission fails. `complete` ignores cancelled, expired, duplicate and unknown
responses. `next_deadline` is `None` when empty, so no idle polling is needed.
Cancellation is local and does not imply a host cancellation RPC.
