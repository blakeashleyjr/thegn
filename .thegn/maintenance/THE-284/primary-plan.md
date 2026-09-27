# Primary review + greenlight — THE-284

Reviewing row 621. **APPROVED with one structural correction — read it before you
write the parser.**

Your evidence is right and one finding is load-bearing: the backlog **already**
carries generation tags and barriers, so the reset points you need exist and you
are not inventing a lifecycle. You also confirmed THE-175's drain change is
already an ancestor, which removes a rebase risk.

## CORRECTION — do not add a second streaming-state mechanism

Your plan says "add a bounded stateful query responder **alongside** the existing
pane-owned clipboard parser". That is the thing to avoid.

THE-244 already solved _this exact problem_ — bounded, per-pane, generation-scoped
retention of an incomplete control sequence across fairness slices — for OSC 52
passthrough. Two independent streaming parsers over the same byte stream, with
their own buffers, bounds and reset points, is how they drift: one gets a reset at
a new lifecycle boundary and the other does not, and the bug reappears in only one
grammar.

So: **read THE-244's implementation first and either share its retention
mechanism or state in your report precisely why the grammars cannot share one.**
"They are different grammars" is not sufficient — a shared bounded-prefix buffer
can feed two matchers. If after reading it you conclude sharing is genuinely
wrong, say so with the reason and proceed; that is an acceptable outcome, an
unexamined second mechanism is not.

## Confirmed as written

- Generation-current admission carried through **both** output branches; stale
  queued tails still feed the emulator in FIFO order but must never complete a
  query across a reattach/fallback boundary, nor deliver a reply into a new
  session. That is the subtle half and you have it right.
- Reset at the existing reattach / fallback / exit points — not at a slice edge.
- Fixed documented bound for an incomplete sequence; discard oversized or
  malformed input through its **semantic terminator** before resuming top-level
  scanning.
- Existing CSI, OSC 10/11 and kitty APC matching and response bytes preserved.
- When `write_reply` refuses, surface the typed reason and **do not claim
  delivery**. A silently dropped reply is indistinguishable from the bug.

## Performance

This is the event loop's hot path. No per-byte allocation, no unbounded buffer —
CLAUDE.md's 0%-idle and <16ms-render invariants apply, and `pty_drain` exists
because a previous audit found inline work in a key handler.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Every lane in the previous chain shipped something that did not
build and the primary caught each — that division of labour is expected, an
optimistic report is not.
