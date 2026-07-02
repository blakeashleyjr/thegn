# Add agent-steerable review (shared live diff surface)

## Summary

Turn the worktree-scoped diff/PR panel into a **shared review surface an agent and
the human drive together**, borrowing from
[`hunk`](https://github.com/modem-dev/hunk) and
[`lumen`](https://github.com/jnsahaj/lumen). Three parts:

1. **Agent-steered navigation** — an agent (over ACP/MCP, through the proxy) can
   navigate the live panel: open the changes/PR section, focus a file, move to a
   hunk — the same panel the human is watching, in-process (no shell escape).
2. **Inline comments both ways** — the agent can attach a comment at a
   file/line/hunk that appears live beside the code; the human's replies/annotations
   are fed back to the agent as its next turn.
3. **Token-lean structured diff projection** — an agent-facing projection returns
   file/hunk structure (from the existing `diff_sbs` parser) with the raw patch
   text **opt-in**, so the agent gets cheap structure without a full patch dump.

## Impact

- **T 262** (inline comments → follow-up prompt) — the comment round-trip is
  exactly this, made interactive and in-process.
- **T 266** (AI change explanation) — the structured projection + steering is the
  substrate for explaining a change hunk-by-hunk.
- **AR 570** (tool-format translation) — the review verbs are advertised as house
  tools, translated per harness.
- Extends the `panel` and `agent` capabilities. **No DB schema change** — comments
  post to GitHub review threads; navigation is transient UI state.

## Rationale

hunk's model — the diff viewer is the _shared artifact_ between human and agent,
driven out-of-band while both watch — is the sharpest idea in the audit that
superzej doesn't have. superzej is already an in-process compositor with a
worktree-scoped panel (`PanelMsg`/`PanelUi`/`PanelData`, `ReviewThreadRow`) and
already parses structured diffs in core (`diff_sbs::parse_unified` →
`SbsFile`/`SbsHunk`), so the projection is nearly free and the steering is just
mapping ACP/MCP verbs onto the existing `PanelMsg` intents. Because superzej owns
the compositor, there is **no shell escape** (lumen routes through `/dev/tty`; hunk
runs a loopback daemon) — the agent and human share the same live panel directly.

This is distinct from `add-agent-review-gate-pipeline` (an _automated_
review→test→lint→PR gate): that runs unattended stages; this is _interactive
co-review_ of the live panel. They compose — the gate can hand findings to the
steerable panel for a human+agent pass. (T262 overlaps; this change owns the
interactive surface, the gate owns the pipeline.)

## Non-goals

- **Building the automated review gate** — that is `add-agent-review-gate-pipeline`;
  this change is the interactive surface it can feed.
- **Granting the agent new authority** — the agent can already run git/shell via
  the bouncer; review comments post through the existing forge path and the
  bouncer's approval gate still applies.
- **A bespoke daemon/socket** — superzej is already the long-running compositor;
  steering rides the existing ACP/MCP transport, not a new loopback server.
- **AI-free-shell dependency** — the panel remains fully usable by the human with
  no agent connected; steering is strictly additive.
