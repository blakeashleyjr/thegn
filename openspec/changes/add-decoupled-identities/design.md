# Design

## The identity primitive

A named identity is a set of **independent, optional** per-tool bindings — any
subset may be set; unset tools fall through (mix-and-match):

```toml
[[identities]]
name = "washu"
git.config  = "~/.config/git/washu"   # → GIT_CONFIG_GLOBAL
git.ssh_key = "~/.ssh/id_washu"        # → GIT_SSH_COMMAND -i … -o IdentitiesOnly=yes
gh.config   = "~/.config/gh-washu"     # → GH_CONFIG_DIR
gpg.home    = "~/.gnupg"               # → GNUPGHOME (shared here)
accounts    = { claude = "washu" }     # → existing account.rs per-provider selection
```

Modeled in `config.rs` as an ordered map (`BTreeMap<String, IdentityConfig>` or a
`Vec` with `name`) with a small `IdentityConfig { git, gh, gpg, ssh_key,
accounts }`. Each field maps to exactly the env var `profile::credential_env`
already emits — identities add **indirection**, not new firewall surface.

## Resolution (per tool, with fallback) — generalizing the firewall

`profile::credential_env(root)` becomes identity-aware. For each tool it resolves,
in order:

1. the tool's binding from the profile's referenced identity
   (`[profiles.<p>].identity`), then a bundle-referenced identity where in scope;
2. **fallback** to today's `<profile_root>/<tool>` path (100% backward compatible
   — an identity-less profile behaves exactly as now).

Each tool resolves **independently**, so `git=washu, gh=personal, gpg=default`
composes cleanly. The firewall invariants are preserved unchanged: forge tokens
(`GH_TOKEN`/`GITHUB_TOKEN`) are still dropped, `GIT_SSH_COMMAND` still forces
`IdentitiesOnly=yes`, and `sandbox_cred_mounts` mounts the **resolved** per-tool
dirs (not blindly `<root>/config/...`).

## Bundle integration (soft scope)

`[bundle.<name>]` gains an optional `identity = "<name>"`. During
`bundle::compose`, a referenced identity folds into the existing
`ResolvedEnv.overrides`/`mounts` exactly where `config_dirs` + `accounts` already
do — so per-pane / per-worktree / per-workspace mix-and-match works through the
existing compose seam with no new resolution path. `config_dirs` remains the raw
escape hatch; `identity` is the named, reusable form.

## Precedence

Reuse the shipped scope precedence unchanged: worktree → workspace → global for
bundle-referenced identities (`bundle::active_chain`), with the **profile's**
`identity` as the base beneath them. A more specific scope overrides a tool it
sets; tools it leaves unset fall through to the less specific identity, then to
the profile-root fallback.

## Switcher UI

An identity switcher palette (`switch-identity` action, e.g. `Ctrl+Alt+i`) lists
`[[identities]]`; selecting one binds it at the chosen scope via `ui_state`
(reusing the bundle-binding KV, e.g. `identity[:ws:|:wt:]`). A status chip shows
the active git/gh identity for the focused pane — the git/gh/gpg/ssh analogue of
the planned account chip (656). Binding is a `ui_state` KV write, **no SQLite
schema change ⇒ no `user_version` bump**.

## Event loop / rendering

Switcher + chip are overlay/chrome changes ⇒ `Full` frame via
`render_plan::plan`. Identity resolution happens at **pane spawn** (already
off-loop, on the compose path), never on the render loop; no new wake source, no
polling — 0%-idle preserved.

## Help

New interactive surface: the identity switcher + status chip. Add a
`docs/help/` page (or section) for identities with an `actions:` entry claiming the
`switch-identity` action id (the help ratchet requires every `ACTION_SPECS` id to
be claimed by a page); map its help context key to that page.

## Migration / compatibility

Zero-migration: with no `[[identities]]` and no `identity =` references, every
profile and bundle resolves to today's paths byte-for-byte. Identities are purely
additive. The three-profile setup (personal/washu/hubone) can adopt a single
shared identity today and split any one tool later by editing one identity field.

## Alternatives considered

- **Extend `Bundle` in place** (approach A) — the fast first step, and this design
  keeps it working; but a bundle is pane-scoped and can't express the
  _profile-level_ firewall base, so a named identity referenced by both is the
  durable target (approach B).
- **Identity subprofiles** (approach C) — in-process identity swap via the
  subsystem trait; deferred as a follow-up once identities exist, since it needs
  teardown/rebind semantics beyond credential resolution.
