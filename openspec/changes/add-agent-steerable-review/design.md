# Design

## Steering channel (agent → panel)

The panel already models interaction as `PanelMsg` intents
(`Open(Section)`, `CursorDown/Up`, `Select`, `ToggleExpand`) mapped from keys by
the pure `accordion_key()`. Add review verbs to the house-tool / ACP surface
(`mcp/router.rs` + `HouseGit`/`HouseForge`, transported via `acp/client.rs`
`AcpInbound`): `review_open`, `review_focus_file(path)`, `review_goto_hunk(idx)`.
The host maps each to the corresponding `PanelMsg` and injects it into the event
loop exactly as a keypress would — so the human sees the panel move live. No new
transport: the verbs ride the existing ACP/MCP path the agent already speaks.

## Structured diff projection (token-lean)

`diff_sbs::parse_unified()` already yields `SbsFile { hunks: [SbsHunk {rows...}] }`
in core. A projection tool `review_structure` returns this as JSON — files, hunk
headers, line ranges — with the **raw patch text opt-in** (`include_patch: bool`).
The default omits patch bytes (cheap context); the agent asks for patch only for
the hunk it's working. Pure and unit-tested (structure without patch is compact;
`include_patch` adds text; empty diff yields empty structure).

## Comments both ways

- **Agent → human**: a `review_comment(path, line, body)` verb posts a review
  comment through the existing forge path (octocrab/gh) and reflects it into
  `PanelData.threads` (`ReviewThreadRow` already carries path/line/resolved) so it
  appears beside the code on the next hydrate. The bouncer's approval gate applies
  as to any write.
- **Human → agent**: the human's replies/annotations on those threads are read
  back and delivered to the agent as its next turn (a `review_replies_since`
  read), closing the loop hunk's model demonstrates.

## Invariants

- **Event loop**: steering verbs arrive on the ACP/control path, which sends on
  the mpsc channel + pulses `TerminalWaker`; the loop applies the `PanelMsg` on
  wake. No new timer, no blocking call on the loop. Comment posting is off-loop.
- **Render**: navigation + a newly-arrived comment are **chrome `dirty`** panel
  repaints, never a pane recompose. render_plan invariants unchanged.
- **State**: no `user_version` bump — comments live in GitHub review threads;
  navigation is transient `PanelUi` state.
- **Additivity**: the panel is fully human-usable with no agent; the verbs are an
  additive AI-layer surface, and the projection lives on the existing core parser.

## Alternatives considered

- **A loopback HTTP daemon (hunk) / `/dev/tty` routing (lumen)** — rejected;
  superzej is in-process and owns the panel, so steering is a direct `PanelMsg`
  injection with no external server.
- **Dumping the full unified diff to the agent** — rejected; the structured
  projection with opt-in patch is the token-lean default (hunk/codag discipline).
- **A new comments table** — rejected; GitHub review threads are the durable store
  and are already hydrated into `PanelData.threads`.
