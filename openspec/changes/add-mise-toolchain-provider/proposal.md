# Add mise as a first-class toolchain provider

Linear: THE-60

## Why

mise support is half-built and undefined at the edges. What exists today:
detection of `.tool-versions` / `mise.toml` / `.mise.toml` / `.nvmrc` maps to
`Tier::ToolVersions` in `thegn-core/src/envplan.rs`; provisioning plans emit
`mise_install_script` (curl-install mise if missing, `mise trust`,
`mise install`) inside sandboxes/hosts; `[toolchain] mode = "mise"` forces the
mise tier for languages-only repos; and the config example _suggests_ wiring
`mise install` into `init_script`/`prepare` by hand. The gaps that make
"full mise support" false:

- **Installed toolchains never reach pane PATH.** After `mise install`, tools
  are only usable if the user's shell rc happens to run `mise activate` inside
  the pane. Nix devshells get first-class injection
  (`[sandbox] inject_devshell`, host-side resolve + cache + PATH prepend);
  mise gets nothing equivalent.
- **`.mise.toml [env]` is never applied** — mise's direnv-like per-directory
  env is a core feature and thegn ignores it entirely.
- **The provisioning script runs `mise trust` unconditionally**, auto-trusting
  a repo-committed config on the user's behalf. mise configs can execute
  arbitrary code at resolution time (`[env] _.source = "script.sh"`, Tera
  templates with `exec()`), so this is a trust decision thegn currently makes
  silently — disconnected from the repo-trust model that gates every other
  repo-authored execution request.
- **Detection covers a fraction of mise's config surface** — none of
  `mise.local.toml`, `mise/config.toml`, `.config/mise/config.toml`,
  `conf.d/*.toml`, or `MISE_ENV` variants (`mise.<env>.toml`).
- **Precedence versus nix devshells and env bundles is undefined** — a repo
  with both a flake and a `mise.toml`, or a pane with a bound bundle, has no
  specified PATH/env layering.
- **No doctor visibility** — no probe for the mise binary, detected configs,
  injection mode, or trust state.

## What Changes

- **Pane injection modes** — new `[toolchain.mise]` table with
  `inject = "auto" | "shims" | "env" | "off"`:
  - `shims` prepends mise's shims directory to pane PATH (below the devshell
    inject slot). Nothing repo-authored executes on the host — safe,
    gate-free. No-op (with a doctor note) when no host mise exists.
  - `env` resolves the full `mise env` on the host, off-loop, cached by a
    config-file content hash (mirroring the devshell-inject pattern) — and is
    **trust-gated**, because host-side resolution executes repo-authored
    config.
  - `auto` (default) = shims always, plus env once the repo's mise config is
    trust-approved.
- **Precedence rules, pinned** (the load-bearing definition):
  - _Provisioning tier_: unchanged — nix declarations (flake/devenv/classic)
    outrank mise; `[toolchain] mode = "mise"` still forces the mise tier for
    languages-only repos.
  - _PATH order in a pane_ (first wins): env-bundle-set PATH entries → repo
    devshell inject → mise shims → curated base PATH.
  - _Env vars_: bundle-set values are never overridden; mise `[env]` values
    fill unset gaps only, and credential-like keys
    (`*_TOKEN`/`*_KEY`/`*_SECRET`/`*_PASSWORD`) are dropped — the same filter
    and low-precedence stance as opt-in `.env` in the env-bundles spec.
    `[env] _.path` entries join PATH at the mise slot.
- **Trust alignment** — the repo's mise config is attacker-authored until
  approved: host-side `mise env` resolution is a `GatedRequest` category
  (`mise.env`) through the `add-config-trust-resolution` TOFU flow, and thegn
  runs `mise trust` only for a config the user has approved (never
  unconditionally). In-sandbox `mise install` during provisioning stays as
  today: it executes contained, the same boundary `inject_devshell`/
  `warm_direnv = "auto"` already cross for repo nix files, and is documented
  as such.
- **Detection surface** — `envplan::detect` and `DETECT_PROBE_SCRIPT` (which
  must move together, per their contract) learn mise's full project-level
  precedence chain: `mise.local.toml`, `mise/config.toml`,
  `.mise/config.toml`, `.config/mise.toml`, `.config/mise/config.toml`,
  `conf.d/*.toml`, and `MISE_ENV` variants; plus the common idiomatic pin
  files mise reads (`.node-version`, `.python-version`, `.ruby-version`,
  `.go-version`, `.java-version`) alongside the existing `.nvmrc`.
- **Doctor probe** — `thegn doctor` reports: mise binary presence + version,
  detected config files, effective inject mode, trust state of the repo
  config, and why injection is degraded when it is.
- **Config docs** — every new `[toolchain.mise]` key documented in
  `config/config.toml.example`.

## Impact

- **tasks.md**: AB 354 (preloaded expected tools), AB 359 (per-worktree
  toolchain injection — the nix half is done, this is the mise half), O
  (configuration).
- **Specs**: extends the `sandbox` capability (delta in this change) — the
  devshell-inject and repo-toolchain requirements live there; precedence
  scenarios reference the `env-bundles` spec's compose seam without modifying
  it.
- **In-flight changes**: builds on `add-config-trust-resolution` (adds the
  `mise.env` gated category; no new trust machinery); orthogonal to
  `add-env-setup-ux` (secret store) and `add-oci-runtime-tiers`; the doctor
  probe follows the `mark-unverified-backends` honesty pattern;
  `complete-devcontainer-support` is a sibling environment source — a repo
  with both a devcontainer and mise config gets the devcontainer's container
  shape and mise's toolchain independently, each under its own gate.
- **Capability catalog**: no new externally invokable operation — the doctor
  extension rides the existing `doctor` verb; trust approval rides the
  existing repo-trust flow. No catalog row.
- **Code**: `thegn-core` — `envplan.rs` (detection widen, probe script),
  `toolchain.rs` (`[toolchain.mise]` config + pure precedence/merge logic +
  env filter, unit-tested to the 95% gate); `thegn-host` — a
  `mise_inject.rs` sibling of the devshell-inject path (off-loop resolve +
  cache + waker pulse), `cmd/doctor.rs` probe, `handlers/repo_trust.rs`
  category plumb-through. No DB schema change (approvals reuse `repo_trust`;
  the env cache is a state-dir file like the devshell cache).

## Non-goals

- **Running mise tasks or hooks.** `mise run` works naturally inside a pane
  that has mise on PATH; thegn never executes `[tasks]` or `[hooks]`
  (enter/leave) itself — worktree lifecycle hooks are
  `add-worktree-lifecycle-hooks`' scope.
- **Becoming a version manager.** thegn shells out to mise; it never parses
  tool versions or resolves runtimes in-process.
- **Installing mise on the host.** In-sandbox provisioning installs mise as
  today; on the host, injection degrades to a no-op with a doctor note —
  installing host tools is the user's call.
- **`mise.lock` management** — read-only respect via mise itself; thegn never
  writes it.
