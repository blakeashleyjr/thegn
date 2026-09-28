# Primary decision — THE-691 (unblocks row 660)

**Your refusal is correct and my brief was wrong.** I told you to route these two
commands onto `api call`'s admitted-capture path. You found that `config get` and
`automations test` are deliberately **tolerant** `SourceInspection` — `config get`
exists so an operator can inspect a config that does **not** load, and
`automations test` is store-free by contract. Strict admission would refuse
exactly the cases those commands exist to serve. Routing them through it would
trade a hang for a regression, which is worse.

Thank you for stopping. That was the right call.

## DECISION — guard the SOURCE KIND, not the content

The hang does not come from tolerance. It comes from the **kind of thing being
opened**: a FIFO blocks on `read_to_string` forever regardless of how strict the
parser is. So the guard belongs on the source, and tolerance stays exactly as it
is.

**Authorize a small bounded tolerant reader**, used by both commands:

1. **Refuse a non-regular source** — FIFO, device, socket, or a symlink to any of
   those — **before opening it for read**, with a message naming what was seen
   (e.g. `config source is a FIFO, not a regular file`). Stat it, do not try it
   and hope.
2. **Bound the byte count** and refuse an oversized source with its own specific
   reason. An enormous regular file is the other way to hang.
3. **A malformed but regular file still behaves exactly as today** — parsed as far
   as possible, reported, never refused for being broken. This is the contract the
   commands exist for, and there must be a test that proves it still holds.

So the deliverable is one reader plus two call sites, not an admission change.

**Do not widen the admission seam, and do not make these commands strict.** If you
find a third caller with the same unbounded read, name it in your report rather
than fixing it here.

## The test that matters

Assert the **refusal and its reason**, not elapsed time. A test that passes because
something finished within N seconds will flake and will not catch a regression back
to blocking. Create a real FIFO in a temp dir, point the command at it, and assert
the specific refusal — and separately assert a malformed regular file is still
inspected successfully.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the crate-wide nextest and the ratchets
centrally.

Never report a verdict for code you could not compile; state what you could not
run.
