# IDE Handoff

## ADDED Requirements

### Requirement: One reveal operation behind three doors

File reveal SHALL be a single operation reachable from the CLI
(`open --file`), the control API (`worktrees.open` with optional
`path`/`line`/`col` fields on its existing catalog row and scope — no new
catalog row), and `thegn://open` URLs, all normalizing to one `reveal_file`
intent. The compositor's claim SHALL: focus the workspace; resolve the
worktree by canonicalized path-prefix against the repo's registered worktree
roots (relative paths against the active worktree first, then a unique match
across roots — ambiguity or a miss is reported, never guessed); select that
worktree's tab; and open the file at the line through the editor seam's
existing chokepoint so `[editor]` placement rules apply. Claims arrive via
the model refresh with no new polling, and a claimed reveal produces a full
chrome frame exactly like a workspace focus.

#### Scenario: IDE extension reveals over the control API

- **WHEN** a paired client with the worktree-open scope calls
  `worktrees.open` with a repo, `path`, and `line`
- **THEN** the call is acknowledged as enqueued and the compositor reveals
  the file within approximately one model-refresh tick

#### Scenario: Ambiguous path is refused

- **WHEN** a relative path matches files in two of the repo's worktrees and
  none in the active one
- **THEN** the reveal reports the ambiguity in the status line and selects no
  worktree

### Requirement: thegn URL dispatch is strict and side-effect-limited

`thegn url <link>` SHALL accept only `thegn://open` and `thegn://pair` links,
parsed against a strict parameter allowlist (`open`: `repo` required, `path`,
`line`, `col` optional; unknown parameters are fatal; `line`/`col` bounded
integers). Dispatch MUST be limited to focus/reveal (`open`) and starting the
existing interactive pairing flow (`pair`): no URL parameter SHALL ever be
executed, expanded into a command, or persisted to configuration; `repo`
SHALL resolve only against already-registered repos; `path` SHALL be
canonicalized and rejected when it escapes the resolved worktree root; and a
pairing link SHALL never redeem a token without the user's explicit approval
step. Malformed or foreign links exit non-zero with a one-line error.

#### Scenario: Well-formed open link

- **WHEN** the OS handler invokes
  `thegn url "thegn://open?repo=myrepo&path=src/lib.rs&line=42"`
- **THEN** the link follows the exact `thegn open myrepo --file src/lib.rs:42`
  path, including its resolution-miss exit contract

#### Scenario: Hostile link is inert

- **WHEN** a link carries an unknown parameter, a path that escapes the
  worktree root after canonicalization, or a repo thegn does not know
- **THEN** the dispatcher exits non-zero without focusing, revealing,
  registering, or executing anything

#### Scenario: Pairing stays interactive

- **WHEN** `thegn url "thegn://pair?host=h&port=1&t=<code>"` is invoked
- **THEN** the existing pairing flow starts and no token is issued until the
  user explicitly approves

### Requirement: IDE extensions integrate as paired thin clients only

The control API SHALL be the sole integration surface for IDE-side
extensions: an extension pairs like any thin client (scoped token), speaks
the catalog-projected transports, and uses catalog verbs (`worktrees.list`,
`worktrees.open` including reveal fields, `sessions.*`, `events.subscribe`).
thegn SHALL NOT expose a bespoke IDE RPC, extension socket, or second
authentication table, and SHALL document the handshake, minimum-scope
guidance, and the bidirectional jump-to-file loop in a
`docs/extending/ide-extension.md` recipe. An external extension author MUST
need nothing from this repository beyond that documentation and the public
API.

#### Scenario: Extension handshake uses ordinary pairing

- **WHEN** a VS Code extension integrates with thegn
- **THEN** it obtains access through the standard pairing flow and scoped
  token, and every operation it performs is a catalog verb subject to
  `required_scope`

#### Scenario: No parallel door appears

- **WHEN** the integration surface is audited
- **THEN** no IDE-specific endpoint, socket, or auth mechanism exists outside
  the capability catalog's projections
