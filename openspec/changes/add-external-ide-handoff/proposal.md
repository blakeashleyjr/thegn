# External IDE handoff — deep, bidirectional, on existing seams

Linear: THE-17

## Why

The file-level half of IDE integration is already shipped and specced: the
editor seam (`thegn_core::editor`) resolves _which_ editor through one ladder,
knows the VS Code family's `-g file:N[:M]` and the JetBrains launchers'
`--line N` syntax in one program table, decides pane-vs-detached placement,
and every host "open in editor" path goes through it (AG 405/406/408/410
done). Superset's whole IDE story is a ⌘O that launches the workspace in a
chosen editor — thegn already exceeds that at file level. What "deep" is
missing is three directions:

- **Outbound, project-level (AG 407, open).** There is no "open this
  _worktree_ in VS Code/JetBrains as a project" — the seam only opens files.
  The per-worktree project window is the thing that makes external-IDE +
  thegn-worktrees actually compose.
- **Inbound.** `thegn open <repo>` remote-controls a running compositor via
  the DB `intents` mailbox — but only to workspace granularity. An IDE (or
  script, or terminal hyperlink) cannot say "focus the worktree containing
  this file and show me this line". And thegn already _advertises_ an
  app-scheme URL (`thegn://pair?…`, `core/control.rs`) that no platform
  launcher registers and no argv path dispatches — the scheme exists on paper
  only.
- **Extensions.** THE-17 implies IDE-side presence. thegn's control plane
  (pairing, scoped tokens, catalog-projected HTTP/gRPC + event feed) is
  exactly the surface an IDE extension should consume — but nothing pins that
  an extension is _an ordinary paired thin client_, and no recipe documents
  the handshake. Left unpinned, the first extension will grow a bespoke RPC
  and a second policy surface.

## What Changes

- **Project-level handoff through the editor seam.** The program table
  (`program_profile`) gains project-open knowledge (`code <root>`,
  `idea <root>`, `zed <root>`, …; terminal editors open a center pane at the
  root); a new `open-in-ide` action (palette + sidebar worktree-row menu)
  opens the **active worktree root** in the resolved editor, honoring the
  existing ladder — `[editor] command` template (rendered with the root as
  `{path}`, no line), per-workspace `tool_command("editor")` override,
  `$VISUAL`/`$EDITOR` — and the existing placement rules
  (`spawn_detached_reaped` for GUI editors, center pane for terminal ones).
  No new config keys.
- **Inbound file reveal.** `thegn open <repo> --file <path>[:<line>[:<col>]]`
  enqueues a `reveal_file` intent; the compositor focuses the workspace,
  selects the worktree whose root contains the path, and opens the file at
  the line through the existing editor-seam chokepoint. The control verb
  `worktrees.open` gains **optional** `path`/`line`/`col` fields feeding the
  same intent — no new catalog row; the CLI flag, HTTP/gRPC fields, and
  intent are one operation.
- **`thegn://` becomes real.** Platform launcher artifacts register the
  scheme (freedesktop `.desktop` `MimeType=x-scheme-handler/thegn;` on
  Linux/BSD; `CFBundleURLTypes` in the generated macOS bundle); a `thegn url
  <link>` dispatcher (hidden verb, invoked by the OS handler) strictly parses
  `thegn://open?repo=…[&path=…][&line=…]` into the same open/reveal path and
  routes `thegn://pair?…` into the existing interactive pairing flow. Unknown
  params are rejected (the `parse_app_link` precedent). URL dispatch is
  focus/reveal/pair-redeem **only** — never command execution.
- **IDE extensions are paired thin clients, pinned.** A spec requirement
  makes the control API the _sole_ IDE-extension integration surface: an
  extension pairs like any thin client (scoped token), consumes catalog verbs
  (`worktrees.list`/`worktrees.open` with reveal fields, `sessions.*`,
  `events.subscribe`) and gets no bespoke RPC, socket, or second auth table.
  A `docs/extending/ide-extension.md` recipe documents the handshake and the
  jump-to-file loop in both directions. Shipping actual VS Code/JetBrains
  extensions is explicitly out of scope for this repo (separate publishable
  projects; the contract here is what makes them thin).

## Impact

- **Roadmap:** closes **AG 407** (GUI editor handoff); builds on **AG
  405/406/408/409/410** and the editor seam; advances **A 6** (one core, many
  front doors — the IDE extension is a front-door _consumer_, not a new
  door); relates to **AK 445** (the API it consumes).
- **Specs:** `editor` (ADDED: project-level launch lines; worktree handoff
  chokepoint), `cli` (ADDED: `open --file` reveal — following the
  `add-terminal-presets` precedent of extending `open` additively),
  `macos-app-launcher` (ADDED: URL-scheme registration in both launcher
  artifacts), new capability **`ide-handoff`** (reveal semantics, `thegn://`
  dispatch + safety, extension-client contract).
- **In-flight overlap:** `add-cli-namespaces-and-remote-open` owns the `open`
  grammar and the `intents` mailbox this rides (both already in the tree);
  the `--file` flag follows its conventions. `add-terminal-presets` also
  extends `open` (`--preset`) — flags are orthogonal.
  `complete-control-surface-coverage` (THE-39) owns pairing web page, CORS,
  audit records, and gRPC parity — extension clients depend on it for
  polish, and the optional `worktrees.open` fields must ride its proto work,
  not fork it. The MCP write-tools branch owns MCP projections of state
  tools; nothing here touches MCP. `define-gui-frontend-lane` (THE-40) pins
  the "paired thin client" shape — the IDE extension is that shape's first
  concrete consumer. `add-config-trust-resolution` covers the exec-trust
  story for configured editor commands. Windows scheme registration (registry
  keys) is deferred to the `add-windows-parity` family.
- **No new catalog rows** (`worktrees.open` gains optional fields, gated by
  its existing `required_scope`); **no DB schema change** (`reveal_file` is a
  new intent `kind` string in the existing v34 `intents` table). New action
  id + `docs/help/ide-handoff.md` page claim it (help ratchet).

## Non-goals

- No VS Code/JetBrains extension shipped from this repo — the contract and
  recipe are the deliverable; extensions are downstream consumers.
- No bespoke IDE RPC, no LSP client/server work (that is
  `add-generic-lsp-registry`'s lane), no editor-embedded terminal.
- No command execution reachable from `thegn://` URLs, ever.
- No change to the editor resolution ladder or its config keys.
- No Windows registry work in this change (windows-parity family).
