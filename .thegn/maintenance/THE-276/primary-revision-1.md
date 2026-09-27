# Primary review — THE-276 revision 1 (reviewing rows 638 and 646)

Row 638's implementation is good and the primary verified it: `cargo nextest run
-p thegn-core -p thegn-svc` on the control paths is **161/161 green**, all six
connect sites are configured, the typed error set is distinct and specific, the
bulk `first` vector and its awaited send loop are gone, and you added the
one-byte-at-a-time streaming test I asked for that was not in your original list.

Row 646's adversarial review then found one real gap, and it is correct.

## REQUIRED — amortize the compaction; `drain`-per-push is still quadratic

> `crates/thegn-core/src/control_wire.rs:565-575` compacts with `Vec::drain` on
> every push after any consumed prefix. A short consumed frame followed by a large
> partial frame arriving in small chunks causes quadratic copied bytes.

The reviewer is right, and is also right that this is **not** a demonstrated remote
path today: the WebSocket paths call `decode_message` once per bounded message, so
nothing reachable drives the streaming decoder that way. It is a reusable core
decoder with a complexity gap.

Fix it anyway, for two reasons: the issue's acceptance criterion says plainly
"decoder complexity remains linear for adversarial inputs", and a linear-time
guarantee that only holds for today's callers is the kind of thing a future caller
silently violates.

**Amortize rather than compacting eagerly.** Track the consumed offset and only
compact when the consumed prefix is worth reclaiming — the standard rule is when
it exceeds half the buffer, which bounds total copying to O(n) over the stream.
Do **not** compact on every push, and do not remove compaction entirely (the
buffer must not grow without bound across a long stream).

Your existing "compact no more than once per bounded input batch, reset when fully
consumed" phrasing was the right intent; the implementation compacts more often
than that describes. Make the code match the comment.

**Add the regression that pins it**: feed a short complete frame, then a large
frame in many small chunks, and assert the total bytes copied — or a proxy for it,
such as compaction count — stays bounded. An assertion on wall-clock time would
be a flake; count the operation instead.

## Scope

`control_wire.rs` only. Do not revisit `client.rs` — the transport limits, the
one-frame-per-message rule and the bootstrap reordering are all verified green and
must not move. Do not touch `decode_message`'s semantics; this is the streaming
decoder beneath it.

## Everything else from row 646

The reviewer reported no other production blocker. Keep the rest exactly as it is.

## Validation

Attempt `nix develop --command cargo check -p thegn-core --all-targets` and
`cargo nextest run -p thegn-core control_wire`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary will re-run the 161-test control set and clippy
regardless.

Never report a verdict for code you could not compile; state what you could not
run.
