# Design — file-manager provider seam

## Context

Today's split:

- `thegn_core::yazi` — pure config/argv logic: `bin()` resolution
  (`[drawer] command` → `THEGN_YAZI_BIN` → `yazi`), `config_home()`,
  `ensure_config()` (seed-once `yazi.toml`/`keymap.toml`, managed
  image-preview and git-status blocks, vendored `git.yazi` +
  `tg-drawer-close/editor` plugins, accent-derived `theme.toml`).
- `thegn-host` — `drawer_state.rs` (flag cache, pool, cold-spawn pipeline),
  `queries::drawer_command` (`OSC 5379;close` / `5379;editor;<path>` decode on
  the drawer pane's output), `actions::dispatch_drawer_command`, layout +
  containment (`[drawer] contain`, memory/cpu properties).

The only provider-selection knob is a raw command string. The seam makes the
selection typed, the integrations declared, and the loss on swap visible.

## Seam shape

**Sync trait** (per `provider-seams`: implementations are process-bound —
argv/config construction and byte scanning, no network, no async client):

```text
trait FileManager {                     // thegn-core, object-safe, no I/O traps
    fn id(&self) -> &'static str;                    // "yazi", "custom"
    fn caps(&self) -> FileManagerCaps;
    fn spawn_spec(&self, cfg, cwd) -> DrawerSpawn;   // argv, env, cwd
    fn prepare(&self, cfg) -> Option<PathBuf>;       // private config dir; None = nothing to prepare
    fn apply_theme(&self, cfg, dir);                 // accent-derived theming (caps.themed)
    fn control(&self, bytes: &[u8]) -> Option<DrawerCmd>; // caps.control_channel
}

struct FileManagerCaps {
    git_status: bool,        // VCS linemode integration
    themed: bool,            // accent-derived theme regeneration
    control_channel: bool,   // manager can emit close / open-in-editor commands
    config_isolation: bool,  // private config home, seeded + managed blocks
    image_policy: bool,      // image-preview containment policy is enforceable
}
```

`DrawerSpawn` is plain data (argv + env pairs); the host owns the PTY spawn,
pooling, prewarm, and the systemd containment wrap for **every** kind — a
custom manager is contained exactly like yazi. File-I/O in `prepare` (config
seeding) is the same best-effort std-fs code `yazi.rs` runs today; it stays in
core under the existing coverage carve-out pattern (pure resolution functions
unit-tested; seed I/O smoke-covered).

- **Factory**: `file_manager_for(cfg) -> Box<dyn FileManager>` keyed by
  `DrawerKind` (`config_enum!`). Reserved kinds return no implementation and
  are rejected by `config validate --strict` per the provider-seams contract.
- **Kinds**: `yazi` (default, full caps), `custom` (all caps false; runs
  `[drawer] command` verbatim), `lf`/`broot` reserved — named because they are
  the managers users actually ask for; a future implementation gives them a
  config-isolation + control story of their own.
- **Back-compat resolution**: `kind` unset + non-empty `command` ⇒ `custom`;
  `kind` unset + empty `command` ⇒ `yazi`. `kind = "yazi"` with a non-empty
  `command` is a validation warning (ambiguous; command wins today, the
  warning says to pick one). `resolve_bin`'s precedence for the yazi kind is
  unchanged (`THEGN_YAZI_BIN` → PATH `yazi`).
- **Control channel**: the `OSC 5379` grammar (`close`, `editor;<path>`)
  becomes the seam's `DrawerCmd` vocabulary; the yazi impl recognizes it via
  its seeded plugins. Another manager could implement the same OSC without the
  plugins; `custom` declares no channel, so the host's PTY drain never scans
  its output. The host drawer-toggle chord works for every kind (closing never
  requires the channel).
- **Probe**: cheap and offline — resolve the binary (which/exists), report
  kind, config-home mode (`private`/`system`/`custom path`), and caps.
  `custom` with a missing binary probes `Unavailable("<cmd> not found")`;
  yazi with `THEGN_YAZI_BIN` unset and no PATH yazi likewise.

## Vendor isolation

After the move, the ratchet-style claim (asserted by a unit test, not a new
ratchet file): generic drawer code (`drawer_state.rs`, layout, pool,
containment, the PTY drain) contains no `yazi`-named symbol or literal; all
seeding/plugin/OSC-plugin knowledge lives in the yazi implementation module.
`[drawer]` keys that are yazi-integration knobs (`config_home`,
`image_previews`, `git_status`) are read only by the yazi impl; for other
kinds they are inert and `thegn doctor` says so in the probe notes.

## Alternatives considered

- **Keep `command` as the only knob** (status quo): silently degrades; nothing
  tells the user what was lost; violates the seam rule.
- **Templatize the yazi integration for arbitrary managers** (generic config
  seeding, generic plugin injection): each manager's config/plugin model is
  bespoke (lf ⇒ shell commands, broot ⇒ verbs); a generic template would be a
  fourth config language. Caps-per-implementation is the house answer.
- **Embed a native file tree instead of an external manager**: the sidebar
  tree + preview pane already cover browsing; the drawer's value is a full
  manager UX, which yazi does better than a rewrite. Out of scope.

## Security

- **Subprocess surface**: the drawer already runs an arbitrary user-configured
  binary (`[drawer] command`); the seam narrows nothing but makes containment
  uniform — every kind is wrapped in the drawer systemd scope
  (`contain`/`memory_max`/`memory_swap_max`/`cpu_quota`) exactly as yazi is
  today, satisfying the existing "file tools are memory-capped" requirement.
- **Config trust**: `[drawer] command` and `kind` remain config-trust-governed
  keys (see in-flight `add-config-trust-resolution`); a repo-level overlay
  changing the drawer command is a command-execution vector and must stay
  behind that trust gate. No new key here executes anything the old one
  couldn't.
- **Control channel**: OSC decoding parses untrusted PTY bytes. The grammar
  stays exactly the current strict one (`close` | `editor;<path>`, terminated,
  bounded); `editor` paths are dispatched through the editor seam
  (`panel_util::open_editor`), never a shell. Scanning is disabled entirely
  for providers without the cap — a custom manager cannot drive the chrome.
- **No credentials, no network, no new external door** (no CATALOG change).

## Open questions

- Should `reveal <path>` (drawer jumps to the file selected in the preview
  pane / panel) become a caps op now or when a second real manager lands?
  Leaning: define the caps bit now, implement for yazi only (`ya emit`-style
  invocation), so the trait doesn't churn later.
- Do `lf`/`broot` reserved kinds warrant sub-tables when implemented, or do
  they reuse `config_home`-style keys? Deferred to their implementation change
  (reserved kinds carry no sub-table, per provider-seams).
