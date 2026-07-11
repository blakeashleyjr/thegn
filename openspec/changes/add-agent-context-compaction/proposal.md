# Add agent context compaction (deterministic scrollback templater)

## Summary

Add a deterministic, pure-Rust **log templater** in `thegn-core` that collapses
repetitive pane scrollback into compact template groups before it is fed to an
agent (over ACP) or through the LLM proxy as context — bounding the context (and
token) spend of "read my terminal output." Modeled on
[`codag-drain`](https://github.com/codag-megalith/codag-drain) (a Drain3-style
streaming templater), including its honesty discipline: it only helps on _large_
windows and must not be applied where it hurts.

## Impact

- **AR 541–586** (AI gateway / context fabric) — a context-budget primitive that
  sits between raw scrollback and the agent/proxy, reducing near-duplicate log
  noise before it costs tokens.
- Relates to the token-lean projection in `add-agent-steerable-review` (same
  discipline: cheapest faithful projection).
- Extends the `agent` and `llm-proxy` capabilities. **No DB schema change** — pure
  compute over in-memory scrollback; off by default.

## Rationale

Terminal panes produce large, repetitive scrollback (build logs, test output,
retry storms). Handing that verbatim to an agent burns context for little signal.
codag-drain shows a deterministic templater — group near-identical lines into a
template + a count + a few samples — cuts tokens sharply on large windows while
preserving diagnosability. Crucially, its authors publish the _negative_ result:
on small windows the compressed form is **worse** than raw. So this ships with the
same rule — compaction is applied only above a size threshold, and its
determinism is an invariant (same input ⇒ same groups, in stable order), matching
thegn's "render decision is a pure function" ethos. It lives in core with no
tokio/termwiz deps and is entirely opt-in.

## Non-goals

- **Interpreting the logs** — the templater compresses; it does not diagnose or
  summarize with an LLM (that would spend tokens to save tokens). "What it means"
  is the agent's job.
- **Compressing small windows** — below the configured threshold, raw scrollback
  is passed through unchanged (the documented codag-drain finding).
- **A hosted/streaming service** — this is an in-process, synchronous templater
  over a captured window, not a daemon.
- **AI-free-shell dependency** — pure core compute; only invoked on the agent/proxy
  context path, off by default, so the shell never depends on it.
