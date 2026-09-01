---
id: configuration
title: Configuration
order: 30
actions: [mode-normal, mode-vim-normal, mode-vim-insert, mode-emacs]
---

# Configuration

Behavior lives in `~/.config/thegn/config.toml`. Trusted layers, low to high:
built-in defaults < the config file < the active profile's
`profiles/<name>/config.toml` < `THEGN_*` value overlays < `--set` values or
TOML fragments. The trusted files are TOML; `--config` changes the main file's
path but not its parser. A repo-root `.thegn.toml`, `.thegn.yaml`,
`.thegn.yml`, or `.thegn.json` is a separate untrusted overlay for
`[sandbox]` (trust-clamped), `[keybinds]`, `[notifications]`, `[issues]`, the
`env` selector, and metrics detection/refusal data. TOML wins, then YAML, YML,
and JSON; if multiple readable candidates exist, thegn warns which path won
and which paths were ignored.

`[project.<slug>]` in your own config refines settings for one repo —
including `[project.<slug>.merge_queue]` and
`[project.<slug>.pr_queue]`, which is where a repo whose gate,
integration branch, or review rules differ from your defaults belongs. `thegn
config explain <key>`, run inside the repo, names the layer that won.

The canonical root for repositories is `projects_dir`; the former
`workspaces_dir` spelling remains accepted for three stable releases. The
same compatibility window applies to `[workspace.<slug>]`,
`confirm_delete_workspace`, `sidebar_workspace_sort`, and
`THEGN_WORKSPACES_DIR`; their replacements are `[project.<slug>]`,
`confirm_delete_project`, `sidebar_project_sort`, and
`THEGN_PROJECTS_DIR`. When both spellings are present, the canonical value
wins and validation reports both exact keys. Legacy values are accepted and
warned about, but canonical writes and generated references emit only the
project spellings. Tracker-owned `workspace_id`, `workspace_slug`, and
`project_id` keys are unrelated and remain unchanged.

The file is watched: edits apply live, no restart.

## Highlights

- `[theme]` — `accent` recolors every surface; presets cycle with
  `Ctrl-Alt-t`; color/glyph fidelity degrade automatically per terminal.
- `[keybinds]` (+ `[keybinds.vim_normal]`, `[keybinds.emacs]`) — rebind
  anything; the [[keybindings]] page always shows the _effective_ result.
- **Keymap modes**: Normal (default), VimNormal (with a `Space` leader
  layer, plus a vim-insert passthrough mode), and Emacs. Switch live with
  `Ctrl-Alt-n` / `Ctrl-Alt-v` / `Ctrl-Alt-e`; `keymap_preset =
"vscode"|"jetbrains"` overlays familiar IDE chords.
- `[[actions]]` — custom shell or composite actions, surfaced in the
  [[command-palette]] and bindable.
- `[[agents]]` / `[[tools]]` — the `Alt-w` "what to run" picker entries, and
  the cast a pipeline names. Each entry carries its `command`, and optionally
  its `harness`, `model`, `env` overlay and headless `permissions` — see
  **Agents, models and accounts** below.
- `[[pipeline.stages]]` — the org chart a `/pipeline` Lead reads (stage,
  agent, prompt, concurrency, `next`), with per-stage `model` / `env` /
  `permissions` overrides. Structure only: thegn validates and displays it,
  the Lead executes it. See [[system-monitor]] for the board.
- `[skills]` — embedded and user-authored prose recipes seeded into each
  configured harness's native project directory. See [[skills]].
- `[hooks]` — host-side commands at worktree and session boundaries. Entries
  accumulate across global, workspace, and trusted repo-local configuration;
  see **Worktree lifecycle hooks** below.
- `[editor]` — how editor/IDE handoff opens worktrees and files. `provider`
  selects `auto`, `vscode`, `cursor`, `zed`, `jetbrains`, `nvim_remote`, or
  `emacs`; `THEGN_EDITOR_PROVIDER` overrides the global choice for one run.
  `auto` uses `[[tools]] editor`, then `$VISUAL`/`$EDITOR`, then `vi`.
  A non-empty trusted `command` template (`{path}`, `{line}`, `{col}`) remains
  highest priority. `open_in = "auto"|"pane"|"external"` decides center tab
  vs detached window.
- `[workspace.<slug>] editor = "cursor"` selects a logical provider for one
  workspace and inherits `[editor] provider` when omitted. It is accepted only
  in the trusted user config, never from a repo-local `.thegn.*` overlay.
- `[lsp]` + `[[lsp.servers]]` — the language-server **registry**. The six
  built-ins (rust/typescript/tsx/javascript/python/go) are pre-registered;
  any other `lang` key with its `extensions` registers an arbitrary server
  (`zls`, `clangd`, an in-house DSL server). `command = ""` disables a
  language; `thegn doctor` lists every server and whether its command
  resolves. Servers named in a repo-local `.thegn.*` are ignored (a
  language-server command is untrusted) — declare them in your user config.
- `[merge_queue]`, `[pr_queue]`, `[autopilot]`, `[sandbox]`, `[share]`, `[forward]`,
  `[media]`, `[replay]`, `[lifecycle]` — optional feature groups.

### Issue autopilot

`[autopilot]` is disabled by default. When enabled, it considers only issues
returned by the provider's authenticated “my issues” filter whose status is
`pickup_status` (default `todo`) and whose labels contain the exact
`trigger_label` (default `agent-ready`). `assignee = "me"` is the only accepted
value: the provider, rather than issue text, supplies consent.

Because enabling autopilot can run a command and write to a forge, repository
settings belong in the trusted user configuration under
`[workspace.<slug>.autopilot]`; a repo-root `.thegn.*` file cannot enable it.
`max_concurrent` and `max_attempts` bound work. `agent` selects an existing
`[[agents]]` entry or `agent_command` supplies its command template. A verified
commit is opened as `ready` or `draft`; the existing PR queue remains the owner
of review, CI, and merge. `done_on_merge` updates the matching issue only after
that queue observes a real merge.

## Database migration ownership

The shared state database has a startup-only ownership policy. Its safe default
allows automatic migrations only from a long-lived controller, never from an
ordinary CLI process found through a worktree-local `PATH`:

```toml
[database]
migration_authority = "controller" # controller | any | disabled
migration_executable = "/home/me/.local/bin/thegn" # optional absolute pin
```

With `controller`, worktree commands may use a schema that already matches but
cannot advance it. `disabled` requires migrations to be enabled explicitly for
an upgrade; `any` restores the legacy first-opener behavior. When
`migration_executable` is non-empty, its canonical path must also match the
running executable. Every database-using process holds a shared schema lease
for its lifetime, so a rebuilt controller refuses to migrate until controllers
using the old schema have exited. Stop the old host/daemon, rebuild, then start
the pinned controller to upgrade.

These keys are accepted only from trusted user/profile config (or the
`THEGN_DATABASE_MIGRATION_AUTHORITY` and
`THEGN_DATABASE_MIGRATION_EXECUTABLE` launcher overrides), never from a repo
overlay, so code in a worktree cannot grant itself authority.

## Worktree lifecycle hooks

`[hooks]` contains host-side commands for the six lifecycle events:
`pre_create`, `post_create`, `pre_destroy`, `post_destroy`, `session_start`,
and `session_end`. The same table is valid in your global config, in
`[workspace.<slug>]`, and in a repo-root `.thegn.{toml,yaml,yml,json}`.
Entries accumulate in that order; they do not replace entries from a lower
layer, and declaration order within each event is preserved.

Each event is an array. A string is shorthand for an object with the defaults
for that event:

```toml
[hooks]
pre_create = ["./.thegn/pre-create.sh"]
post_create = [
  { command = "pnpm install --frozen-lockfile", wait = false,
    timeout_secs = 120, on_failure = "warn" },
]
pre_destroy = ["docker compose down"]
post_destroy = []
session_start = []
session_end = []
```

The object form accepts `command`, `wait`, `timeout_secs`, and `on_failure`.
Commands with an empty value are ignored. `timeout_secs` must be greater than
zero and defaults to 120. `wait` defaults to `false` and is valid only for
`post_create`; setting it holds the first pane behind the host-side
post-create completion gate. It never blocks the compositor event loop.

Failure defaults are `block` for `pre_create` and `pre_destroy`, and `warn`
for the other four events. A blocking `pre_create` prevents the git checkout
and registration. A failed `pre_destroy` leaves a user-requested worktree in
place; `wt rm --force` reuses the existing force confirmation to skip that
veto. Unattended merge cleanup and rollback use warn-and-continue semantics.
Repo hooks remain warn-only even after trust approval, so a cloned repository
cannot veto a local operation.

Hook working directories are event-specific: `pre_create` and `post_destroy`
run from the repository root; `post_create`, `pre_destroy`, `session_start`,
and `session_end` run from the worktree. The runner clears the inherited
environment and supplies the curated host baseline plus exactly these context
values: `THEGN_EVENT`, `THEGN_REPO_ROOT`, `THEGN_WORKTREE`, `THEGN_BRANCH`,
and `THEGN_WORKSPACE`. Hook entries do not inherit `env_passthrough`,
`host_env_allow_extra`, credentials, or agent sockets. Output is captured in a
per-worktree state log and failures are surfaced through notifications.

Repo-local hook tables are trust-on-first-use gated. Until the existing repo
trust request for `hooks.<event>` is approved, those entries are omitted and
the pending request is reported; approval does not change their warn-only
failure policy. The legacy `[sandbox].prepare` list is still accepted as the
first global `post_create` entries (and repo `sandbox.prepare` as the first
repo `post_create` entries), using the same timeout, logging, notifications,
and failure rules. It is no longer a separate fire-and-forget mechanism.

`session_start` is scheduled once when the first pane for a worktree session is
about to spawn, and `session_end` once when its last pane exits or the tab
closes. Neither delays pane creation or tab close, and these latches are
process-local rather than stored in SQLite. `[sandbox].init_script` is
different: it remains a per-pane script executed inside the sandbox, not a
lifecycle hook.

## Session forking

Live daemon sessions can be forked with `thegn session fork` or the
`fork-session` pane action. Forking uses the daemon's retained launch recipe
and current harness capabilities; it has no additional TOML setting.

## Agents, models and accounts

An `[[agents]]` entry is _what runs_; four optional keys say _how_:

```toml
[[agents]]
name = "reviewer"
command = "claude"
harness = "claude"            # the launch shape: claude | codex | pi | aider
model = "claude-opus-5"       # → `claude --model claude-opus-5` on every launch
env = { CLAUDE_CONFIG_DIR = "file:~/.thegn/accounts/review" }   # pin an account
permissions = ["Read", "Edit", "Bash", "Grep", "Glob"]        # headless allow-list
```

- **`harness`** decides the headless form (`claude -p …`, `codex exec …`,
  `pi -p …`, `aider --message …`) and the model flag. Unset, it is inferred
  from the command's program name. `provider` is the older spelling; set one.
- **`model`** is appended through the harness's own flag (`--model`, `-m`).
  A model on a harness thegn has no flag for fails `thegn config validate` —
  it is never silently dropped.
- **`env`** is applied last, so it wins over the composed identity env. Values
  expand `env:VAR` and `file:PATH`; never write a raw secret here. This is how
  one entry runs under a second account (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) or
  a relocated pi home (`PI_CODING_AGENT_DIR`).
- **`permissions`** is seeded into the worktree before the harness starts
  (claude: `.claude/settings.local.json` → `permissions.allow`, every other key
  in that file kept), so a headless worker never auto-denies its first tool.

A stage overrides any of these for its own launches — including `harness`, so
one generic role can run on claude for reviews and pi for the fan-out — and
`thegn session open --stage <name>` applies them:

```toml
[[agents]]
name = "pipeline-pi"
command = "pi"
harness = "pi"
model = "model-proxy/standard"     # pi models are `provider/id`

[[pipeline.stages]]
name = "code"
agent = "pipeline-pi"
model = "model-proxy/fast"         # the fan-out tier runs cheaper
concurrency = 3
next = "review"
prompt = "Implement {parent_artifact} in {worktree}; summarise to {artifact}."

[[pipeline.stages]]
name = "review"
agent = "pipeline-pi"
harness = "claude"                 # this stage swaps harness; model rides its flag
model = "claude-opus-5"
prompt = "Review {parent_artifact}; verdict to {artifact}."
```

```sh
thegn session open --agent pipeline-pi --stage code \
  --worktree ~/wt/app/fix --prompt "Implement chunk 1" --json
thegn doctor            # "[[agents]] (effective)" — harness · model · env keys · permissions
thegn config validate   # a model on a flagless harness, a bad env key, a
                        # harness/provider disagreement: all reported here
```

Edits to `[[agents]]` take effect on the next launch — the daemon re-reads
its config per agent launch, so no restart is needed.

## Skills

```toml
[skills]
enabled = true
user_dirs = ["~/.config/thegn/skills", "./team-skills"]
exclude = ["mq"]
```

- **`enabled`** controls automatic seeding during worktree creation and
  startup reconciliation. It defaults to `true`; `thegn skills seed` remains
  an available explicit operation when it is `false`.
- **`user_dirs`** names additional package roots. Each immediate,
  non-symlink child directory may contribute `<name>/SKILL.md`; discovery is
  bounded and non-recursive, and embedded entries win duplicate names. The
  list defaults empty. Invalid or unreadable entries produce diagnostics and
  do not prevent other packages from loading.
- **`exclude`** is a duplicate-free list of path-safe skill names withheld
  from every harness. A previously seeded file is removed only while its
  managed marker and hash prove that it has not been edited.

Skills go into the selected worktree's native Claude, Codex, or Pi project
layout, never into a harness home. Harness targeting, feature gates, package
frontmatter, and conflict rules are detailed on [[skills]]. The generated
[[config-reference]] carries the same defaults and inline key documentation.

## Inspecting

```sh
thegn config show        # the effective merged config
thegn config get ui.language          # any dotted key; --json for real types
thegn config set merge_queue.regenerate_paths '["Cargo.lock", "pnpm-lock.yaml"]'
thegn config explain merge_queue.gate_command   # value + which layer set it
thegn config validate    # strict validation; reserved provider kinds are
                          # reported by name
thegn doctor             # resolved terminal capabilities + every provider's probe
thegn keys list          # every binding, grouped by zone (--json, --zone)
thegn keys validate      # chord conflicts; exits non-zero, so it fits a hook
thegn keys hints --zone sidebar   # what that zone's hint strip renders
```

Some provider `kind` values are **reserved**: the name is accepted so a
config stays forward-compatible (for example `[ci] provider = "drone"`,
`[[forges]] kind = "forgejo"`, `[media] backend = "jellyfin"`), but this build
has no implementation behind it. A reserved value loads with a warning and
falls back to the default; `thegn config validate` rejects it by name, and
`thegn doctor` lists it as unavailable with the reason.

## Layers, env vars, unknown keys

Settings resolve in a fixed order: built-in defaults → your `config.toml` →
the active profile overlay → `THEGN_<SECTION>_<KEY>` environment variables →
`--set key=value` on the command line. A repo's selected `.thegn.*` overlays
`[sandbox]`, `[keybinds]`, `[notifications]`, `[issues]`, the `env` selector,
and trust-gated `[hooks]`; a metrics table is recognized for the existing
refusal diagnostic. Every load is tolerant: a malformed value warns and the
layer below stands, so a typo never blocks a launch. Explicit
`thegn config validate` checks every layer it can locate and exits non-zero
for a problem.

Env overrides exist for the knobs a CI job or launcher would flip —
`THEGN_BASE_BRANCH`, `THEGN_SANDBOX_BACKEND`, `THEGN_THEME_COLOR`,
`THEGN_LOG_LEVEL`, … (`thegn config explain <key>` shows whether an env var
set it). Not every key has one; the full list is the `env_overlay` table in
the source, and a new key either gets a knob or is deliberately recorded as
not having one.

Unknown keys are dropped on load with a warning. `thegn config validate`
reports them with a nearest-key hint and names the file and dotted key
(`sandbox.enabeld: unknown key (did you mean `enabled`?)`) — run it after
editing by hand. It also checks the active profile and selected repo overlay
when those files exist; missing optional layers are quiet. The generated
config reference contains every documented key with its example value; those
values are illustrative and are not promised to equal code defaults.

The home-manager module (`programs.thegn.*`) renders a `config.toml` with
the same keys; its options are checked against the schema in CI, so it
cannot offer a value the binary rejects.

`keys list` covers the keymap registry **and** the zone-local tables — the
sidebar's row keys and each panel section's row-mode keys. Those are handled
by the focused zone rather than the registry, so they are not rebindable, but
they are listed so nothing is hidden. See [[keybindings]] for the rebindable
set.

The complete key-by-key documentation is the generated
[[config-reference]] — schema/example coverage and generated-key coverage keep
it from drifting from the shipped example.
