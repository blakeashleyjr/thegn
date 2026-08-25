# Design — external IDE handoff

## What exists

- **Editor seam** (`thegn_core::editor`, `openspec/specs/editor/spec.md`):
  ladder (`[editor] command` → `[[tools]] editor` → `$VISUAL` → `$EDITOR` →
  `vi`), one program table (`program_profile`: `+N` / `file:N[:M]` /
  `-g file:N[:M]` / `--line N`), placement (`Pane` vs `External`,
  `spawn_detached_reaped`), one host chokepoint (`panel_util::open_editor`),
  doctor-probed. File-granular only.
- **Remote open** (`cli` spec, `cmd/open.rs`): `thegn open <repo>` enqueues a
  `focus_workspace` intent in the DB `intents` mailbox (v34), claimed by the
  compositor's model refresh within ~1s, claim-and-delete, last-wins; with no
  live instance it sets the active-workspace pointer and launches.
- **App scheme on paper**: `PairingUrl::app_link()` emits
  `thegn://pair?host=…&port=…&t=…` and `core/control.rs::parse_app_link`
  parses it strictly (unknown params rejected, tested) — but no launcher
  registers `x-scheme-handler/thegn`, no `CFBundleURLTypes` exists in the
  generated bundle, and no argv path dispatches a URL.
- **Control plane**: pairing + scoped tokens, catalog-projected HTTP/gRPC,
  `worktrees.list`/`worktrees.open` (`SurfaceSet::ALL`), SSE event feed.
  THE-39 (`complete-control-surface-coverage`) is driving it to 100% coverage
  with audit records and a served pairing page.

## Outbound: project-level handoff

- `ProgramProfile` gains a project dimension: whether the program opens a
  directory as a project and with what argv (`code <root>`, `idea <root>`,
  `zed <root>`, `subl <root>`; terminal editors get `Pane` placement with cwd
  = root, e.g. `hx .`). Pure table + launch-line logic in `thegn-core`,
  unit-tested like the existing jump-syntax table.
- `EditorRequest` grows a project form (root, no line). A configured
  `[editor] command` template is rendered with the root as `{path}` and empty
  line/column — template users keep working with zero new keys; templates
  that hard-require a line render without one (the placeholder contract
  already tolerates absent line/column).
- New action `open-in-ide`: command palette + the sidebar worktree-row menu
  (opens _that_ row's worktree, not just the active one). Placement and
  reaping are the seam's existing rules; nothing new on the loop (a detached
  spawn).
- Per-workspace override rides the existing ladder untouched
  (`tool_command("editor")`, AG 410).

## Inbound: reveal

- **One operation, three doors.** CLI flag (`thegn open <repo> --file
<path>[:<line>[:<col>]]`), control verb fields (`worktrees.open` gains
  optional `path`/`line`/`col`), and URL (`thegn://open?repo=…&path=…&line=…`)
  all normalize to one `reveal_file` intent payload
  `{repo, path, line?, col?}`. No new catalog row; `worktrees.open`'s
  existing `required_scope` gates the control-surface form.
- **Intent kind choice**: a separate `reveal_file` kind rather than extending
  `FocusIntent`, because focus-style intents are last-wins per kind — a
  reveal must not be swallowed by a plain focus racing it, and vice versa.
  No schema change (kind is a string; the `intents` table exists).
- **Compositor claim**: focus the workspace; resolve the worktree by
  canonicalized path-prefix against the repo's registered worktree roots
  (relative paths resolve against the active worktree first, then unique
  match across roots; ambiguity or a miss → status-line error, never a
  guess); select that worktree tab; open the file at line/col through
  `panel_util::open_editor` — so `[editor] open_in` placement applies and a
  GUI-editor user gets file-in-IDE while a terminal-editor user gets a center
  pane. This is deliberately the same chokepoint as every other jump (AG 408).
- **Launch path**: with no live instance, the pointer + launch behavior of
  `thegn open` is unchanged; the reveal intent is enqueued first and claimed
  by the freshly-launched compositor's first model refresh.

## `thegn url` dispatch and scheme registration

- Registration is data in existing launcher artifacts:
  `MimeType=x-scheme-handler/thegn;` (+ `%u` in `Exec=`) on the freedesktop
  `.desktop` entry; `CFBundleURLTypes` in the `make-app.sh`-generated
  `Info.plist`. Both are owned by the `macos-app-launcher` capability's
  installer artifacts — an ADDED requirement there, no behavior of the
  existing registration requirement changes. Windows registry registration is
  explicitly deferred (windows-parity family).
- `thegn url <link>` (hidden from grouped help, like legacy verbs): strict
  parse; `thegn://open` → the exact `cmd/open.rs` path (resolution misses
  list candidates and exit 3, same contract); `thegn://pair` → the existing
  interactive pairing redeem flow, still requiring the user's explicit
  approval in the TUI/CLI — a URL can _start_ pairing, never complete it
  silently. Anything else (unknown host, unknown params, non-thegn scheme)
  exits non-zero with one line.
- Not a new capability-catalog door: `url` is a local dispatcher onto
  operations that already exist (`worktrees.open` semantics, pairing verbs);
  it adds no remotely invokable operation. Stated here so the catalog rule is
  answered, not skipped.

## IDE extensions as control-API consumers

- **The contract, pinned**: an IDE extension is an ordinary paired thin
  client. Handshake = pairing URL / `thegn://pair` link → scoped token;
  transport = the catalog-projected HTTP + SSE feed (gRPC where a client
  prefers it); vocabulary = catalog verbs only. No thegn-side extension
  socket, no bespoke RPC, no second auth table — the same "zero new policy
  surface" judgment THE-39 made for a web GUI.
- **Jump-to-file both directions** falls out: IDE→thegn is
  `worktrees.open{path,line}` (or shelling `thegn open --file`); thegn→IDE is
  the editor seam (`open-in-ide`, per-file jumps). The
  `docs/extending/ide-extension.md` recipe documents both, plus
  `events.subscribe` for worktree-list changes so an extension's picker stays
  live.
- **Scope guidance in the recipe**: request the minimum (read + the
  worktree-open scope); mutating calls surface in THE-39's audit records.
- Shipping extensions is out of scope: this repo's lane is the contract; a
  marketplace extension is a downstream consumer that must not require thegn
  changes to exist — that property is the test of this design.

## Event loop, rendering, schema (config.yaml design rules)

- **Wake path**: nothing new polls. The reveal intent rides the existing
  model-refresh claim (~1s); `thegn url` and `open-in-ide` are short-lived
  CLI/detached-spawn paths off the loop. No new background thread.
- **Damage channel**: a claimed reveal changes chrome (workspace/tab focus,
  possibly a new center pane) ⇒ master `dirty`, a `Full` frame — identical to
  `focus_workspace` today. `open-in-ide` external spawns cause no frame
  change at all.
- **SQLite**: no schema change, no `user_version` bump (`reveal_file` is a
  new intent-kind string in the existing table; hydration tolerance rules
  unchanged).
- **Help context key**: new page `docs/help/ide-handoff.md` claims the
  `open-in-ide` action id (help + prose ratchets). No new `zone:*`/`panel:*`
  context — the action lives in the palette and the sidebar row menu, whose
  contexts keep their existing pages; the sidebar page gains a one-line
  mention of the row-menu entry.
- **e2e**: no new chrome to freeze; the sidebar row menu gains one entry, so
  affected muse baselines re-record with `just e2e-update` if any spec shows
  that menu.

## Security

- **`thegn://` is an unauthenticated local door — treat every link as
  hostile.** Browsers prompt before invoking scheme handlers, but a drive-by
  page can still present one click. Therefore: dispatch is focus/reveal/pair
  _only_; no URL parameter is ever executed, expanded into a command, or
  written to config; parsing is strict allowlist (unknown params fatal, the
  `parse_app_link` precedent); `repo` resolves only against already-known
  repos — a URL can never register, create, or scan for one; `path` is
  canonicalized and prefix-checked against the resolved worktree root
  (symlink escapes and `..` traversal land outside the root and are
  rejected); `line`/`col` are bounded integers. Worst case achievable by a
  malicious link: your own thegn focuses a file you already have, or shows a
  pairing-approval prompt you decline.
- **Pairing links never auto-redeem.** `thegn://pair` starts the existing
  flow; token issuance still requires the user's explicit approval step. No
  token, secret, or credential ever appears in a `thegn://open` URL.
- **Outbound spawns are an exec surface** resolved from config
  (`[editor] command`, `[[tools]] editor`) — covered by the config-trust
  story (`add-config-trust-resolution`); argv is rendered with shell-quoted
  substitution, spawned detached and reaped; a worktree root is the only
  data interpolated.
- **Extension clients**: scoped tokens (SecretRef-style custody on the client
  side is the extension's problem, but the recipe says so); server-side
  blast radius is bounded by `required_scope` per verb; no CORS is opened by
  this change (browser-hosted clients remain THE-39's `[serve] cors_origins`
  decision); mutating verbs ride THE-39 audit records.
- **Reveal over the control API** is remote-controlled UI focus — annoying if
  abused, but requires a paired token with the worktree-open scope; rate
  concerns are bounded by intent last-wins semantics per kind.

## Alternatives considered

- **Bespoke IDE plugin protocol** (dedicated socket/JSON-RPC): rejected — a
  second policy surface and a parallel vocabulary the catalog already
  provides; the whole point of THE-39 is one list.
- **Outbound jump via per-IDE URL schemes** (`vscode://file/<abs>:l:c`,
  `jetbrains://…/navigate/reference`): considered as a fallback when no CLI
  launcher is on PATH; rejected for v1 — the program table's CLI launchers
  are more reliable and already specced; MAY be revisited as a program-table
  fallback column later.
- **Shipping a reference VS Code extension in-repo**: rejected — different
  toolchain, different release cadence, and the contract's quality bar is
  precisely that an external author needs nothing from this repo but docs.
- **Extending `FocusIntent` instead of a new intent kind**: rejected —
  last-wins-per-kind would let a plain focus swallow a reveal.

## Open questions

- Should `open-in-ide` prefer window reuse (`code -r`) when the program table
  knows the flag? Leaning yes as a program-table detail, not a config key.
- JetBrains inbound: their launchers are project-openers, and file jumps
  (`--line`) are covered — is `jetbrains://` navigate worth a fallback column
  if the Toolbox launcher is absent? Deferred with the per-IDE-scheme
  alternative.
- Remote (ssh/provider) worktrees: `code --remote ssh-remote+host <root>` is
  a natural extension of project-open for rows whose location is remote —
  scoped out here (touches the provider seam), noted for the follow-up.
