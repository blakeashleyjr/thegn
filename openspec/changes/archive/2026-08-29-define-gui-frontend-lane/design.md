# Design — native GUI frontend lane decision

The complete decision and evidence record is
`docs/superpowers/specs/2026-08-29-native-gui-frontend-lane-design.md`.

## Decision

THE-40 builds no GUI. If reopened, the native product lane is a separate GPU
cell client attached to the daemon and control API. It does not become a
feature-gated renderer inside `thegn-host`, and the shell never depends on it.
Native chrome waits for a serializable view model. Browser access is separate
remote-access work.

This archived change is documentation only. Dependency bans and owner-table
rules are implementation policy to land with a future frontend crate, not
changes made here.

## Substrate findings

- `sessions.attach` is cataloged, routed, and protocol-versioned. Observer and
  interactive attach kinds already permit multiple subscribers; only
  interactive clients resize.
- The stream supplies `Hello`, snapshot, and sequenced delta frames, and the
  bounded daemon subscriber recovers from lag by sending a fresh snapshot.
- Control-v1 documents the route and JSON parameters, but not the complete
  binary frame variant/header/sequence contract. A stable GUI client still
  needs a compatibility-tested fixture/schema and reconnect policy.
- Serve mode already supplies one-time pairing, scoped tokens, a pairing page,
  and configured exact-origin CORS. Plaintext TCP still requires a trusted
  network or tunnel, so public browser deployment remains separate work.
- The daemon registry is flat. Layout, tabs, sidebar, panels, statusbar,
  overlays, focus, and hit targets remain compositor-owned and have no
  serializable server-side view model.

## Candidates

| Candidate                       | Benefit                                                                                                                             | Primary cost                                                                                                                            | Decision                                                                                                            |
| ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| 1. GPU terminal-embedding shell | Reuses TUI cells and provides app presence quickly.                                                                                 | Adds another terminal/window boundary and distribution surface without semantic chrome; an in-host version forks the compositor.        | Reject as the product lane; existing launchers already cover app presence.                                          |
| 2. Native control client        | Reuses daemon PTYs, warm attach, observer mode, scoped catalog actions, and can eventually provide native input/rendering features. | Requires terminal emulation, frame compatibility, reconnect/resync, input/resize policy, packaging, and—only later—a chrome view model. | Preferred future lane. Begin with an observer cell grid and add native chrome only after a serialized model exists. |
| 3. Web client for serve mode    | Natural remote/mobile consumer of HTTP, SSE/WebSocket, pairing, CORS, and tokens.                                                   | Browser fidelity, input, reconnect, token handling, and public TLS/security are a separate product.                                     | Defer to THE-39/remote access; do not bundle it into a webview to call it native.                                   |

## Constraints

- The TUI retains its blocking 0%-idle loop; a client waits on its own
  transport and creates no compositor polling source.
- All actions project `thegn_core::capability::CATALOG` and existing scopes.
  There are no GUI-only verbs, routes, policies, token types, or direct DB/PTY
  handles.
- Core and host remain independent of toolkit, GPU, font, window, and terminal
  emulator substrates. A future client owns those at a leaf boundary.
- `[daemon] enabled = false`, TUI launch, detach, degradation, and CLI use work
  with no GUI installed or running.
- THE-34 owns event filters and opt-in lag vocabulary. THE-40 consumes that
  contract and does not add another subscription or state feed.
- Native chrome waits for THE-43 or its successor to expose a stable,
  serializable component/view model.

## Follow-up

THE-40-F1 publishes the observer cell-client contract. It consumes
`sessions.list` and `worktrees.list`, attaches one observer, decodes hello /
snapshot / delta, renders one cell grid, and pins reconnect, sequence, lag,
geometry, and version-skew fixtures. It uses THE-34 events only for resync
hints. It adds no toolkit, chrome, config, capability, migration, or web
surface.

Only after that spike, finalized interactive ownership semantics, a chrome
view model, and a documented need unmet by terminal emulators should candidate
2 be reconsidered as an implementation.

## Validation and ratchets

This decision has no render/event-loop effect and no security surface. It adds
no dependency, route, config, capability, state, or code; consequently it
changes no ratchet. Any future implementation introduces its frontend crate,
dependency ownership, tests, and relevant ratchet updates together.
