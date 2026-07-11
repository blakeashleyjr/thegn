# Design

## The templater (core, pure, deterministic)

A new `thegn-core::compact` module implements a Drain-style templater:

- **Tokenize** each line (with a character-class fallback for punctuation-heavy /
  compact-JSON lines, per codag-drain's adaptation).
- **Group** lines that match the same template (fixed tokens + variable slots).
- **Emit** each group as `{ template, count, samples: [..], slots: [..] }` in
  **ascending first-occurrence order** — never iterating a hash map for emission,
  so output is deterministic (same input ⇒ identical output).

`compact(lines, cfg) -> Compaction` is **pure + unit-tested**: repeated lines
collapse to one group with the right count; distinct lines stay distinct; slot
extraction captures the varying token; a compact-JSON line falls back to the
char-class tokenizer; emission order is stable across runs.

## The size gate (honesty discipline)

`compact` is applied to a scrollback window only when the window exceeds a
configured line/byte threshold. Below it, the raw window is returned unchanged —
encoding codag-drain's published finding that compression _loses_ on small
windows. The threshold and an on/off switch are config; default off.

## Wiring (opt-in, on the context path)

When enabled, the ACP/proxy context feed (e.g. a "read this pane's output" tool or
scrollback handed to the agent) routes the captured window through `compact`
before it becomes agent context. The proxy's `proxy_requests` cost log lets the
before/after token effect be observed. Nothing else changes; with the switch off,
raw scrollback flows as today.

## Invariants

- **Event loop**: `compact` is synchronous pure compute invoked on the
  context-preparation path (already off the render loop); no timer, no blocking
  I/O on the loop.
- **Render**: none — this does not touch rendering.
- **State**: no `user_version` bump — pure compute, no persistence.
- **Additivity**: lives in core with no tokio/termwiz/proxy deps; only _invoked_
  from the AI context path, off by default. The AI-free shell never calls it.

## Alternatives considered

- **LLM summarization of scrollback** — rejected; spends tokens to save tokens and
  is non-deterministic. A deterministic templater is cheaper and reproducible.
- **Always compact** — rejected; codag-drain's own eval shows a loss on small
  windows, so the size gate is mandatory.
- **A separate crate/service** — unnecessary; an in-process pure module fits the
  synchronous "compress this captured window" use and keeps the 95% core gate.
