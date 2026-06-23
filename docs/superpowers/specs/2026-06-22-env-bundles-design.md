# Design: Environment Bundles (.env / dotfile / profile management)

## Context

superzej can already swap **one** thing per coding-agent:
`crates/superzej-core/src/account.rs` relocates a provider's credential home
(`CLAUDE_CONFIG_DIR` / `CODEX_HOME`) per worktree/workspace/global using a
precedence chain over the `ui_state` KV table. That proves the per-scope-binding
pattern — but it is narrow: it moves a single env var, for a _known_ agent,
never touching shell panes, git identity, dotfiles, or arbitrary env.

At the other end, `docs/superpowers/specs/2026-06-11-profiles-subprofiles-design.md`
designs **heavyweight profiles**: a whole separate OS process with a
rerooted-env firewall (work vs personal as separate windows). That is the _hard_
isolation boundary — coarse (per-process), and unbuilt.

This document designs the **missing middle**: named **environment bundles** —
composable units of env vars + credential/config-dir redirection + optional
dotfiles + per-provider account selection — that **bind at any scope** (global,
workspace, worktree, repo) and inject at the existing pane-spawn seam. So
"work vs personal" can differ _within one running process_, and "multiple Claude
profiles" falls out as the first consumer. This rides the **AI-free shell** track
(strictly additive; never a hard AI dependency).

```
account.rs            env bundles (this doc)            heavyweight profiles
─────────────         ──────────────────────            ────────────────────
one env var,          arbitrary env + dotfiles +        whole-process firewall,
one provider,    ───▶ accounts, bound at any      ───▶  rerooted env, one
per scope             scope, one process                window per profile
(narrow)              (the soft middle layer)           (the hard boundary)
```

**This document is design-only.** It defines the model, config schema, binding/
precedence, the composition seam, the dotfile tiers, the `.env` opt-in,
secret resolution, and verification. No implementation steps are prescribed.
Tracked as roadmap group **AU** (`tasks.md`).

Decisions locked with the user:

1. **Complementary lighter layer.** Bundles bind per worktree/workspace inside
   ONE process and inject at the pane seam. Heavyweight process-profiles remain
   the hard firewall; bundles **generalize** `account.rs`, they do **not**
   replace the firewall.
2. **All three dotfile tiers, cheapest as default** — config-dir redirection
   (Tier 1, default), materialized dotfiles (Tier 2, opt-in), full synthetic
   `$HOME` (Tier 3, opt-in).
3. **Named bundles + opt-in `.env`** — config bundles are the base; a worktree
   may additionally opt into its own `.env`, gated by a direnv-style allowlist.
4. **`env:` indirection + external secret-resolver hooks** — keep
   `expand_env_ref`'s `env:VAR`; add pluggable resolvers so secrets never land in
   superzej config/DB in plaintext.

---

## Core model — the env bundle

A **bundle** is a named, declarative unit in config (`[bundle.<name>]`). All
fields are optional; an empty bundle is the no-op identity.

```toml
[bundle.work]
extends      = ["base"]                 # optional named composition (low→high)

# Arbitrary env vars. Values support env:/secret-resolver indirection (§ Secrets).
env          = { ANTHROPIC_BASE_URL = "https://proxy.internal", FOO = "env:HOST_FOO" }

# Per-provider account selection — delegates to account.rs for the cred-home dir.
accounts     = { claude = "work", codex = "work" }

# Tier 1 (default): redirect config-dir env vars. No file ops.
config_dirs  = { GIT_CONFIG_GLOBAL = "~/.config/git/work", GH_CONFIG_DIR = "~/.config/gh-work" }

# Tier 2 (opt-in): materialize dotfiles into the bundle's managed HOME.
[bundle.work.dotfiles]
source = "~/dotfiles/work"              # symlink/template tree → managed HOME
mode   = "symlink"                      # "symlink" | "template"

# Tier 3 (opt-in): run panes rooted at a synthetic HOME for this bundle.
home   = "managed"                       # "managed" | "<path>"; absent ⇒ inherit real HOME

# .env opt-in toggle (§ .env).
dotenv = true
```

Fields:

- **`env`** — explicit key/values, the most general lever.
- **`accounts`** — `{ <provider> = <account-name> }`, resolved through
  `account::account_dir()` to the credential home and injected as that provider's
  home env var (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`). This is how `account.rs`
  becomes a _consumer_ of bundles instead of a parallel system.
- **`config_dirs`** — Tier-1 redirection: a curated set of well-known config-dir
  env vars (`GIT_CONFIG_GLOBAL`, `GH_CONFIG_DIR`, `GNUPGHOME`, agent homes). Just
  env, no file operations.
- **`dotfiles`** — Tier-2 materialization spec (§ Dotfile tiers).
- **`home`** — Tier-3 synthetic HOME (§ Dotfile tiers).
- **`dotenv`** — opt into loading the worktree's `.env` on top (§ .env).
- **`extends`** — names of other bundles merged first (low precedence), for
  composition (`work` extends `base`).

Bundles live in config (declarative, reviewable, diffable). Per-scope **bindings**
(which bundle is active where) live in the DB `ui_state` table, exactly like
account pointers.

---

## Binding & precedence

Reuse `account.rs`'s mechanism verbatim, generalized from `account:<p>:…` to
`bundle:…`. The active binding at each scope is resolved most-specific-first:

1. **Worktree override** — `ui_state["bundle:wt:<path>"]`
2. **Workspace config** — `[workspace.<slug>].env_bundle`
3. **Workspace pointer** — `ui_state["bundle:ws:<slug>"]`
4. **Global active** — `ui_state["bundle"]`
5. **None** — no bundle; panes inherit the curated base env only.

This mirrors `account::active_name()` / `set_active()` and the
`Bind { Global, Workspace, Worktree }` enum (`account.rs:182-241`) — the same
helpers are lifted to operate over bundle scopes.

**Layered merge vs. single bundle.** Two composition axes, both ordered
low→high with per-key override:

- **Scope layering** — the bundles bound at _global → workspace → worktree_ are
  merged (not replaced), so a worktree bundle refines the workspace bundle rather
  than discarding it.
- **`extends`** — within a single bundle, named parents merge first.

Effective env = `base curated env` ◁ `extends chain` ◁ `global bundle`
◁ `workspace bundle` ◁ `worktree bundle` ◁ `.env (opt-in, filtered)`, where
`◁` is per-key override by the right-hand side. (`.env` is deliberately _below_
bundle credentials in precedence — see § .env.)

---

## Composition seam — `env::compose()`

A new core function (`crates/superzej-core/src/env.rs`) is the single resolution
point:

```rust
pub struct ResolvedEnv {
    pub overrides: Vec<(String, String)>,  // KEY=VALUE to set
    pub block:     Vec<String>,            // keys to unset inside the child
    pub mounts:    Vec<sandbox::Mount>,    // path-preserving cred/home mounts
}

pub fn compose(
    cfg: &Config, db: &Db,
    worktree: &str, slug: Option<&str>,
    choice: Option<&str>,        // agent choice, or None for a plain shell pane
) -> ResolvedEnv
```

`compose()` **subsumes** the account-injection and scoped-key logic currently
inlined in `agent::launch_spec_with_key()`:

- resolves the active bundle by precedence, applies `extends`,
- expands `env:`/secret references (§ Secrets),
- folds `accounts` through `account::account_dir()` into provider home env vars,
- folds `config_dirs` and Tier-2/3 HOME into `overrides` + `mounts`,
- returns blocked keys (e.g. masking a master `ANTHROPIC_API_KEY` when a scoped
  key/account replaces it — the existing scoped-key behavior).

The returned `ResolvedEnv` maps 1:1 onto the existing
`SandboxSpec.{env_overrides, env_block, mounts}` fields and `wrap_script()` —
**no new sandbox mechanism**. `agent.rs` routes its `LaunchSpec` through
`compose()` instead of calling `account::launch_env()` directly.

**Shell panes too.** Today only _agent_ choices get account env; a plain shell
pane inherits whatever szhost inherited. `compose()` is called for **every** pane
spawn (`choice = None` for shells), so a shell pane in the `work` worktree sees
the work gitconfig/git identity, not the launching shell's. This is the change
that makes "different profiles per worktree" real for ordinary terminals, not
just agents.

---

## Dotfile tiers

Three tiers, with **Tier 1 as the implicit default** (a bundle that only sets
`env`/`config_dirs`/`accounts` is Tier 1).

- **Tier 1 — config-dir redirection (default, no file ops).** The bundle points
  well-known env vars at alternate dirs: `GIT_CONFIG_GLOBAL`, `GH_CONFIG_DIR`,
  `GNUPGHOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`. Tools read the alternate config
  with zero copying. Cheapest, safest, covers the "work vs personal git identity
  - agent login" case entirely.

- **Tier 2 — materialized dotfiles (opt-in).** `[bundle.<n>.dotfiles]` symlinks
  or templates a source tree into a **managed per-bundle HOME** under
  `$XDG_STATE_HOME/superzej/bundles/<slug>/home/`. Materialization is
  **idempotent** (hash the source, re-link only on change) and runs **off the
  event loop** on a background thread handing back over a channel — same pattern
  as the diff fs-watcher's recursive inotify registration — to preserve the
  ~0%-idle / <16ms-render invariants. The managed HOME is bind-mounted
  path-preserving into the sandbox via `ResolvedEnv.mounts`.

- **Tier 3 — full synthetic HOME (opt-in).** `home = "managed"` sets `HOME` (and
  `XDG_*` as needed) to the bundle's managed HOME, so panes run fully rooted
  there. Strongest soft isolation; most invasive. Layered on Tier 2's materialized
  tree.

Managed-HOME layout:

```
$XDG_STATE_HOME/superzej/bundles/<slug>/
  home/          # Tier 2 materialized dotfiles; Tier 3 HOME root
  meta.json      # source hash, last-materialized ts (idempotency)
```

---

## `.env` opt-in & allowlist

Named bundles are the base. A worktree may **additionally** opt into loading its
own `.env`, gated direnv-style:

- **Discovery** — only when the active bundle (or a per-worktree toggle) sets
  `dotenv = true`. superzej never auto-loads a repo's `.env` silently.
- **Allowlist** — first load (and any time the file content hash changes)
  requires an explicit allow, stored as `ui_state["dotenv:allow:<path>"] =
<content-hash>`. A changed hash re-prompts (the file may now contain hostile
  vars). No allow ⇒ not loaded.
- **Precedence** — `.env` loads at **low precedence**: it fills _gaps_ but never
  overrides bundle-set values, and in particular never overrides credentials the
  bundle established.
- **Credential-key filter (security boundary).** Keys matching `*_TOKEN` /
  `*_KEY` / `*_SECRET` / `*_PASSWORD` (configurable) are **dropped** from `.env`
  by default — a repo's `.env` cannot inject a token into an agent's environment.
  Document this prominently; it is the main reason `.env` is opt-in and filtered
  rather than direnv-equivalent.

---

## Secrets

Keep `expand_env_ref()` (`config.rs:27-44`) for `env:VAR`, and extend the same
expansion point with **pluggable resolver schemes** so secret _values_ never sit
in superzej config or the DB in plaintext:

```toml
[secrets.resolvers]
pass   = "pass show {ref}"
sops   = "sops --decrypt --extract '[\"{key}\"]' {file}"
op     = "op read {ref}"               # op://vault/item/field
agenix = "agenix -d {ref}"
cmd    = "{ref}"                        # arbitrary command (explicit opt-in)
```

A bundle value like `ANTHROPIC_API_KEY = "pass:work/anthropic"` is resolved by
running the mapped resolver at **launch time**, **off the event loop**. Results
are injected into the child env and **never persisted** (not in `ui_state`, not in
`proxy_requests`, not logged). Resolution failure **degrades gracefully**: warn,
skip that key, continue (the agent then falls back to its own auth or fails
loudly itself) — never block the spawn or the loop.

---

## Multiple Claude profiles — the worked example

The headline consumer. A `work` bundle:

```toml
[bundle.work]
accounts    = { claude = "work" }                       # → CLAUDE_CONFIG_DIR=<work login>
env         = { ANTHROPIC_BASE_URL = "https://proxy.work" }   # optional proxy/virtual-key
config_dirs = { GIT_CONFIG_GLOBAL = "~/.config/git/work" }    # work git identity
```

Bind it to the work workspace:

```toml
[workspace.acme-monorepo]
env_bundle = "work"
```

…and bind `personal` to personal worktrees via the switcher. Now:

- launching `claude` in an `acme-monorepo` worktree uses the **work** login
  (`CLAUDE_CONFIG_DIR` → work account dir, path-preserving-mounted into the
  sandbox), the work git identity, and the work proxy endpoint;
- a personal worktree uses the personal login + identity;
- **switching is a hot-swap** — it affects agents/shells launched _after_ the
  switch; running sessions keep their environment until relaunch (identical
  semantics to `account.rs` and roadmap item 656).

This is precisely "build out our multiple claude profiles" — each Claude profile
is a bundle's `accounts.claude` selection plus whatever env/identity rides with
it, bound where you want it.

---

## Firewall relationship & caveats

Bundles are **soft isolation**, by design:

- They share the **clear-then-allowlist base env** prerequisite with the
  heavyweight-profile firewall: `pane.rs::spawn_with_env` currently inherits the
  _entire_ parent env and only adds `TERM`/`COLORTERM` on top
  (`pane.rs:108-119`), so a shell pane sees every var szhost inherited. Bundles
  need a **curated base** (`PATH`, `HOME`, `TERM`, `LANG`, `USER`, …) with bundle
  env layered on top — otherwise the launching shell's creds leak past the
  bundle. This fix lands here and is reused by the process-profile firewall.
- The same sandbox caveats apply: `file_access = all` / `--dev-bind / /`
  (`sandbox.rs`) and `--network host` defeat _any_ env isolation. Bundles are
  only as strong as the loosest sandbox/file-access setting.
- **Bundles do not replace process profiles.** For a true firewall (separate DB,
  audit log, kernel-enforced singleton, cross-profile state isolation), use the
  heavyweight profiles (group H). Bundles give per-worktree/workspace
  _convenience_ isolation within one window.

---

## Critical files (where the implementation would land)

- `crates/superzej-core/src/env.rs` — **new**: `compose()`, `ResolvedEnv`, bundle
  resolution + precedence (generalized from `account.rs`), secret-resolver
  dispatch, `.env` load + filter.
- `crates/superzej-core/src/account.rs` — refactored into a _consumer_ of
  `env::compose` (account selection becomes a bundle field); precedence helpers
  generalized to bundle scopes.
- `crates/superzej-core/src/config.rs` — `[bundle.*]` schema (`Bundle` struct),
  `[secrets].resolvers`, `[workspace.<slug>].env_bundle`; extend
  `expand_env_ref` for resolver schemes.
- `crates/superzej-host/src/agent.rs` — route **every** pane spawn (agent _and_
  shell) through `env::compose`; `launch_spec_with_key` delegates to it.
- `crates/superzej-host/src/pane.rs` — `spawn_with_env` clear-then-allowlist base
  env (shared with the profile firewall).
- `crates/superzej-core/src/sandbox.rs` — reuse `env_overrides`/`env_block`/
  `mounts`; bundle managed-HOME mounts ride the existing path-preservation.
- `crates/superzej-core/src/db.rs` — `ui_state` bundle bindings + `.env`
  allowlist hashes (no schema bump); a v16 table only if managed-bundle metadata
  outgrows `ui_state`.
- Tier-2/3 materialization — a background worker (off-loop, channel + waker),
  mirroring the diff fs-watcher.
- UI — status-bar bundle chip (extend the account chip, item 656) + palette
  command to bind the active bundle at worktree/workspace/global scope.

---

## Verification (for the eventual implementation)

1. **Resolution/precedence unit tests** (core, gated 95%): mirror
   `account.rs`'s `active_name_precedence` for bundle scopes; assert
   global→workspace→worktree layering and `extends` merge order, per-key override.
2. **Compose unit tests**: given a bundle, assert `ResolvedEnv.overrides/block/
mounts`; assert `accounts` folds to the right provider home var + mount;
   assert master-key masking via `block`.
3. **Cred-leak test**: spawn a shell pane (`choice = None`), dump env, assert the
   launching shell's `GH_TOKEN`/`SSH_AUTH_SOCK`/API keys do **not** survive, and
   `git config user.email` returns the _bundle's_ identity.
4. **`.env` security test**: a worktree `.env` containing `FOO=bar` and
   `SECRET_KEY=x` — assert `FOO` loads only after allow, never overrides a
   bundle-set `FOO`, and `SECRET_KEY` is filtered out by default.
5. **Secret-resolver test**: a fake `cmd:` resolver returns a value; assert it
   reaches the child env and appears in **no** persisted store/log; assert a
   failing resolver warns + skips without blocking the spawn.
6. **Idle-CPU test**: Tier-2 materialization on a large dotfile tree introduces
   **no** main-loop wakeups (`SUPERZEJ_LOG` waterfall / 0%-idle check); it runs
   off-thread and hands back over a channel.
