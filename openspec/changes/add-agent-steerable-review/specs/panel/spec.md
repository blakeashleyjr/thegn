# Panel

## ADDED Requirements

### Requirement: An agent can steer the live review panel the human is watching

superzej SHALL let an agent, over its existing ACP/MCP transport, navigate the
worktree-scoped review panel — open the changes/PR section, focus a file, move to
a hunk — by mapping review verbs onto the panel's existing interaction intents and
applying them on the event loop, so the human sees the same panel move live. The
panel MUST remain fully usable by the human with no agent connected, and steering
MUST NOT bypass the event-loop/render path (a steering action is a chrome repaint,
not a pane recompose).

#### Scenario: Agent focuses a file and the human sees it

- **WHEN** an agent issues a review verb to focus a file in the panel
- **THEN** the live panel moves to that file for the human, as if navigated by key

#### Scenario: Panel works with no agent

- **WHEN** no agent is connected
- **THEN** the review panel is fully usable by the human via the existing keys

### Requirement: The diff is available to agents as a token-lean structured projection

superzej SHALL expose an agent-facing structured projection of the diff — files,
hunk headers, and line ranges — derived from the existing unified-diff parser,
with the raw patch text included only on explicit opt-in, so an agent gets cheap
structure without a full patch dump. An empty diff MUST yield an empty projection.

#### Scenario: Structure without patch is compact

- **WHEN** an agent requests the diff projection without opting into patch text
- **THEN** it receives file and hunk structure with no raw patch bytes

#### Scenario: Patch text is opt-in

- **WHEN** an agent requests the projection with the patch opt-in for a hunk
- **THEN** the raw patch text for that hunk is included

### Requirement: Review comments flow both ways between agent and human

superzej SHALL let an agent attach a review comment at a file/line that posts
through the existing forge path (subject to the bouncer's approval gate) and
appears beside the code in the panel, and it SHALL feed the human's replies on
those threads back to the agent as its next turn. No new persistence is required —
comments live in the forge's review threads already hydrated into the panel.

#### Scenario: Agent comment appears in the panel

- **WHEN** an agent posts a review comment at a file and line
- **THEN** after approval the comment is posted to the forge and shown beside that
  code in the panel

#### Scenario: Human reply returns to the agent

- **WHEN** the human replies to an agent's review comment
- **THEN** the reply is delivered to the agent as its next turn
