# Primary review — THE-284 revision 1 (reviewing row 631, commit f4810e07)

You did the right thing: you implemented the issue, and you reported that my
structural correction was **not** satisfied rather than quietly declaring it done
or silently dropping it. Your account of the cost is also specific enough to act
on — a shared scanner needs explicit event dispatch that preserves OSC 52's own
validation and retry ownership.

**On that evidence the primary is changing the requirement. You do not have to
build the shared scanner.**

## DECISION — keep the two parsers, share the RESET

My correction was aimed at a failure mode, not at an architecture: two streaming
parsers over one byte stream **drift when one gets a lifecycle reset and the other
does not**, so the bug reappears in only one grammar. Re-reading your report, the
buffering duplication is not what creates that risk — the reset wiring is. And
refactoring OSC 52's validation and retry ownership, which THE-244 landed
recently, to serve a tidiness argument is a bad trade.

So the requirement is narrowed to the part that actually prevents the drift:

1. **One reset call site.** A single function resets _all_ per-pane streaming
   parser state — the clipboard parser and the query parser together — and every
   lifecycle boundary (reattach, fallback, exit, generation change) calls only
   that. No boundary may reset one parser directly.
2. **A test that a generation boundary resets BOTH.** Feed each parser a partial
   sequence, cross the boundary, and assert neither can complete across it. One
   test, both grammars, so a future parser added without wiring is visible as a
   failure rather than as a latent bug.
3. **A comment at each parser** naming the other and pointing at the shared reset
   site, saying plainly that a third parser must be registered there. That comment
   is what carries the decision forward; without it this review is lost.

If a shared reset is somehow also infeasible, that is a different and more
surprising claim — report it with the reason and stop.

## Confirmed from your report, keep as implemented

- Bounded per-pane CSI/OSC/APC parsing across slices.
- Generation admission through **both** output branches, with stale queued tails
  still feeding the emulator in FIFO order but never completing a query across a
  boundary.
- **Truthful Full/Closed reply warnings.** This is the detail I most wanted and
  you have it: a refused `write_reply` surfaces the typed reason and does not claim
  delivery. A silently dropped reply is indistinguishable from the original bug.
- The regression set: fairness slicing, generation barriers, corner relay, input
  refusal.

## Performance

This is the event loop's hot path: no per-byte allocation, no unbounded buffer.
`pty_drain` exists because an earlier audit found inline work in a key handler —
do not reintroduce any.

## Scope

The two parsers, their reset wiring, and the tests. Nothing else. Do not modify
OSC 52's validation or its retry ownership — that is THE-244's landed behaviour and
preserving it is the reason this revision is narrow.

## Validation

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report a verdict for code you could not compile; state what you could not
run.
