# Design — mise toolchain provider

## Shape: mirror the devshell-inject seam, don't invent a second one

The nix devshell already solved this problem's structure: resolve on the host
(where the daemon/tools live), cache by content hash, merge into pane env at
compose time, never block a spawn. mise injection is the same seam with a
different resolver:

- **Resolver**: `mise env -s json` (env mode) — run `sh -lc` in the worktree,
  off-loop, output cached under `$XDG_STATE_HOME/thegn/mise/<hash>` keyed by a
  hash over every detected mise config file's content (so an edit invalidates).
- **Shims mode** needs no resolver at all: the shims directory
  (`~/.local/share/mise/shims`, overridable via `MISE_DATA_DIR`) is prepended
  to PATH at compose time. Tools appear as soon as `mise install` (in-sandbox
  provisioning, or the user by hand) has populated them.
- **Merge point**: the same pane-spawn compose seam the env-bundles spec pins
  ("A single compose seam resolves env for every pane") — mise is one more
  layer in that fold, not a parallel mechanism.

Pure logic (config parsing of thegn's own keys, precedence merge, credential
filter, cache-key derivation) lives in `thegn-core` (95% gate); everything
that executes mise lives in `thegn-host` (`mise_inject.rs`), exercised by
smoke.

## Precedence (the rules, and why)

**Provisioning tier** (which system materializes toolchains) is already a
total order in `envplan::tier()`: Nix > ToolVersions > SynthNix > Languages >
Bare, with `[toolchain] mode` able to force the mise tier. Unchanged — a repo
that declares a flake has chosen nix; mise files alongside it do not demote
the flake.

**PATH within a pane** (first wins):

1. env-bundle-set entries — the user's explicit identity/config layer;
2. repo devshell inject (`inject_devshell`) — the repo's declared toolchain;
3. mise shims — version-pinned fallbacks; shims are interceptors, so they must
   sit _below_ the devshell: a repo that pins `node` via flake must not have a
   mise shim shadow it;
4. curated base PATH.

**Env vars**: bundle-set values are never overridden (bundles are
user-trusted; mise config is repo-trusted at best). mise `[env]` fills unset
gaps only, with the env-bundles `.env` credential filter applied
(`*_TOKEN`/`*_KEY`/`*_SECRET`/`*_PASSWORD` dropped). This gives one uniform
story: _repo-authored env, from any source (.env, mise, devcontainer), is
low-precedence and credential-filtered._

Ties into `thegn config explain`: the mise layer reports as its own origin in
the clamp/provenance trace, so "why is `FOO` set?" has an answer.

## Trust model (Security — load-bearing)

The threat: a cloned repo's `mise.toml` executes code when _resolved on the
host_ — `[env] _.source = "./x.sh"` sources a script, Tera templates can run
`exec()`, and plugin-backed tools run arbitrary install code. mise's own
answer is its trust prompt; today thegn _bypasses_ it by running `mise trust`
unconditionally inside the provisioning script.

Rules:

- **Shims mode executes nothing repo-authored on the host** — prepending a
  directory to a pane's PATH runs no code — so it is the gate-free default.
  (What runs _inside the pane_ is repo code territory anyway; the sandbox is
  the boundary there, same as for any shell.)
- **Host-side `mise env` resolution is gated**: a `GatedRequest` category
  `mise.env`, canonical-form matched over the detected config file set's
  content hash, through the same TOFU flow as `.thegn.toml` overlays and
  devcontainer categories. Unapproved ⇒ not resolved, surfaced pending, pane
  spawns with shims only. An edited mise config re-prompts.
- **`mise trust` follows thegn's approval**: the provisioning script and the
  host resolver invoke `mise trust <file>` only for configs the user approved
  in thegn. thegn never widens mise's trust store beyond its own.
- **In-sandbox `mise install` stays ungated by default**: it runs contained,
  with the worktree bind and the sandbox's network policy — the same boundary
  `warm_direnv = "auto"` and `inject_devshell` already cross for repo nix
  files. Users who want it gated tighten the sandbox, not a mise-specific
  knob.
- **Credential filter** on applied `[env]` values (above), and resolved env is
  never persisted to config/DB/logs — the cache file holds it (state-dir,
  0600), same exposure class as the devshell cache.
- **Blast radius**: no new write surface; no credentials handled; the doctor
  probe is read-only.

## Event loop / damage / persistence

- Resolution runs on a background thread (QoS `Utility`), sends on the
  existing refresh channel and **pulses the `TerminalWaker`**; a cold cache
  never blocks a spawn (pane opens with shims; env applies on later spawns
  once warm) — the devshell-inject contract verbatim.
- No new render channel: pending-trust and degradation notices ride existing
  notification/status chrome (Full frame on dirty).
- No SQLite schema change: approvals reuse `repo_trust`; the env cache is a
  state-dir file.
- No new interactive surface (no action/keybind/zone/panel section); config
  keys documented in `config/config.toml.example`, picked up by the generated
  config-reference help page.

## Alternatives considered

- **`mise activate` in pane rc** (status quo suggestion): depends on the
  user's dotfiles, invisible to thegn, breaks clear-then-allowlist spawning.
  Rejected as the _mechanism_; still works for users who prefer it (inject
  `off`).
- **Running `mise env` inside the sandbox per pane**: adds per-spawn latency
  and requires mise in every image; host-side resolve + cache matches the
  devshell pattern and works for bwrap/host panes too.
- **Gating shims mode too**: prepending a PATH dir executes nothing on the
  host; gating it would train users to click through prompts for a safe
  operation.
- **A full provider seam (`thegn_core::seam`) for toolchain managers**: mise
  is one binary with a stable CLI; the seam machinery (caps/probe/reserved
  kinds) is warranted when a second manager (asdf? vfox?) arrives. The config
  is shaped so `[toolchain.mise]` can become `[toolchain.<manager>]` without
  breakage; noted as future work, not built now.

## Open questions

- Should `MISE_ENV` be settable per-worktree (worktree-scoped envs like
  `mise.staging.toml`)? Deferred: respect the host env var if present;
  a `[toolchain.mise] env = "…"` key can come later.
- Whether `mise.lock` freshness should factor into the env cache key
  (currently: config-content hash only; the lock is content too if present).
