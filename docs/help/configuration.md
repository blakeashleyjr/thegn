---
id: configuration
title: Configuration
order: 30
actions: [mode-normal, mode-vim-normal, mode-vim-insert, mode-emacs]
---

# Configuration

Behavior lives in `~/.config/thegn/config.toml`. Layers, low to high:
built-in defaults < the config file < `THEGN_*` environment variables <
CLI flags. A repo-root `.thegn.{toml,yaml,yml,json}` overlays per-repo
settings (sandbox, keybinds, env selection). CI autofix is only permitted in
trusted user configuration: `[workspace.<slug>.ci] mode = "suggest"`
or `"auto"`; a repo-authored file cannot enable it.

`[workspace.<slug>]` in your own config refines settings for one repo —
including `[workspace.<slug>.merge_queue]` and
`[workspace.<slug>.pr_queue]`, which is where a repo whose gate,
integration branch, or review rules differ from your defaults belongs. `thegn
config explain <key>`, run inside the repo, names the layer that won.

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
- `[editor]` — how "open in editor" opens files: `command` is a template
  (`{path}`, `{line}`, `{col}`); unset, thegn uses `[[tools]] editor`, then
  `$VISUAL`/`$EDITOR`, then `vi`, composing each program's own line-jump
  syntax. `open_in = "auto"|"pane"|"external"` decides center tab vs
  detached window (auto: windowed editors detach).
- `[lsp]` + `[[lsp.servers]]` — the language-server **registry**. The six
  built-ins (rust/typescript/tsx/javascript/python/go) are pre-registered;
  any other `lang` key with its `extensions` registers an arbitrary server
  (`zls`, `clangd`, an in-house DSL server). `command = ""` disables a
  language; `thegn doctor` lists every server and whether its command
  resolves. Servers named in a repo-local `.thegn.*` are ignored (a
  language-server command is untrusted) — declare them in your user config.
- `[ci]` — cached, bounded CI run/log reads. `log_cache_runs = 0` disables
  persistence; `[ci.autofix] mode = "off"|"suggest"|"auto"` defaults to off
  and reuses the existing trusted PR-queue agent policy.
- `[merge_queue]`, `[pr_queue]`, `[sandbox]`, `[share]`, `[forward]`,
  `[media]`, `[replay]`, `[lifecycle]` — optional feature groups.

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

## Inspecting

```sh
thegn config show        # the effective merged config
thegn config get ui.language          # any dotted key; --json for real types
thegn config set merge_queue.regenerate_paths '["Cargo.lock", "pnpm-lock.yaml"]'
thegn config explain merge_queue.gate_command   # value + which layer set it
thegn config validate    # --strict also rejects *reserved* provider kinds
thegn doctor             # resolved terminal capabilities + every provider's probe
thegn keys list          # every binding, grouped by zone (--json, --zone)
thegn keys validate      # chord conflicts; exits non-zero, so it fits a hook
thegn keys hints --zone sidebar   # what that zone's hint strip renders
```

Some provider `kind` values are **reserved**: the name is accepted so a
config stays forward-compatible (for example `[ci] provider = "drone"`,
`[[forges]] kind = "forgejo"`, `[media] backend = "jellyfin"`), but this build
has no implementation behind it. A reserved value loads with a warning and
falls back to the default; `thegn config validate --strict` rejects it by
name, and `thegn doctor` lists it as unavailable with the reason.

## Layers, env vars, unknown keys

Settings resolve in a fixed order: built-in defaults → your `config.toml` →
`THEGN_<SECTION>_<KEY>` environment variables → `--set key=value` on the
command line. A repo's `.thegn.toml` can overlay `[sandbox]` only. Every
layer is tolerant: a malformed value warns and the layer below stands, so a
typo never blocks a launch.

Env overrides exist for the knobs a CI job or launcher would flip —
`THEGN_BASE_BRANCH`, `THEGN_SANDBOX_BACKEND`, `THEGN_THEME_COLOR`,
`THEGN_LOG_LEVEL`, … (`thegn config explain <key>` shows whether an env var
set it). Not every key has one; the full list is the `env_overlay` table in
the source, and a new key either gets a knob or is deliberately recorded as
not having one.

Unknown keys are dropped on load with a warning. `thegn config validate
--strict` reports them with a nearest-key hint
(`sandbox.enabeld: unknown key (did you mean `enabled`?)`) — run it after
editing by hand.

The home-manager module (`programs.thegn.*`) renders a `config.toml` with
the same keys; its options are checked against the schema in CI, so it
cannot offer a value the binary rejects.

`keys list` covers the keymap registry **and** the zone-local tables — the
sidebar's row keys and each panel section's row-mode keys. Those are handled
by the focused zone rather than the registry, so they are not rebindable, but
they are listed so nothing is hidden. See [[keybindings]] for the rebindable
set.

The complete key-by-key documentation is the generated
[[config-reference]] — it can never drift from the shipped example.
