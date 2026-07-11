# Seamless dev-shell tooling inside sandboxes (and for contributors)

## Context

thegn runs each worktree's interactive process in a sandbox (podman → docker
→ bwrap → none), with the host toolchain (`/nix/store`, `$HOME`) bind-mounted
**read-only** and — for OCI/bwrap backends — **no nix daemon socket** mounted
(`sandbox.rs:566` already documents this as a known limitation). The compositor
(`thegn`) itself runs on the host with full nix access.

Two friction points fall out of this:

1. **Sandboxed panes lack the project toolchain.** A worktree pane — whether an
   interactive shell or a non-interactive agent — does not get the repo's
   `nix develop` devShell tools (`shellcheck`, `yamllint`, `taplo`, `treefmt`,
   `cargo-llvm-cov`, …) on `PATH`. Inside the sandbox you cannot `nix develop`
   to fix this: the store is read-only and there is no daemon, so nix cannot
   realize the devShell. Result: `just lint`, `just coverage`, `nix fmt`
   silently fail or skip gates. This was hit directly while verifying a change:
   clippy/cargo worked (global profile) but shellcheck/yamllint/taplo did not.

2. **Contributor onboarding assumes manual `nix develop`.** There is no
   auto-activation (no direnv, no thegn-native injection) and no
   environment-doctor, so a fresh clone runs `just ci`/lint/fmt only after the
   contributor knows to enter `nix develop` first.

**Goal:** make the repo's devShell toolchain available **out of the box** inside
sandboxed worktree panes (the case automated agents hit) and to contributors,
without relaxing sandbox isolation by default.

### Decisions already made (with the user)

- **Tier A (read-only, always-on):** provide the _already-built_ devShell tools
  on `PATH` without any store writes → no daemon needed, zero isolation cost.
  Covers lint / fmt-check / test / coverage.
- **Tier B (opt-in):** a config flag that bind-mounts the nix daemon socket into
  the sandbox so full `nix develop`/`build`/`fmt` work inside it. Off by default
  (preserves the hardening model).
- **Mechanism:** Approach 1 — **thegn-native dev-env injection** (resolve on
  host, inject into pane). Not direnv-dependent (a committed `.envrc` is a
  convenience only). Not a GC-rooted profile (that is Approach 1 plus an extra
  moving part).
- **Phasing:** ship Tier A + onboarding first; Tier B daemon flag is an
  independent fast-follow.

## Architecture

A new **dev-env resolver** in `thegn-core` (substrate-agnostic, testable) that
the host consults when spawning a worktree pane:

```
repo has flake devShell?  ──no──▶  no-op (today's behaviour)
        │ yes
        ▼
host: nix print-dev-env --json   (writable store + daemon live here)
        ▼
cache: $XDG_STATE_HOME/thegn/devenv/<hash>.json   (hash = flake.lock + flake.nix)
        ▼
host: merge exported vars (PATH first) into pane env   ← before the sandbox exec
        ▼
sandbox pane: every process sees the devShell tools (store paths RO-mounted)
```

The resolver is a small unit with one job (resolve + cache a repo's devShell
env), a clear interface (`repo path → map of exported env vars`), and one
dependency (the `nix` binary, behind the subprocess seam). It does **not** know
about panes, sandboxes, or the event loop.

## Tier A — resolve + inject (core deliverable)

### Resolve

- Run `nix print-dev-env --json` **on the host** for the repo root. The JSON
  shape is `{ "variables": { NAME: { "type": "exported"|"var"|…, "value": … } } }`.
  Extract the **exported** variables — primarily `PATH`, plus project vars the
  shellHook exports (e.g. `THEGN_YAZI_BIN`). JSON avoids fragile shell-script
  parsing and is language-agnostic (no assumption the pane runs bash).
- Degrade silently: if `nix` is absent, the repo has no `devShell`, or the
  command fails/non-zero, return `None` and log at `info`. No user-facing error.

### Cache

- Path: `$XDG_STATE_HOME/thegn/devenv/<hash>.json`.
- `<hash>` = content hash of `flake.lock` + `flake.nix`. Any flake change
  invalidates; otherwise a warm read is ~free.
- Stale entries are harmless (overwritten on the next hash change); a simple
  size/age cap can prune later if needed (not v1).

### Build off the event loop

- `print-dev-env` is a multi-second subprocess. It runs on a **background
  thread** (same pattern as the diff fs-watcher / hydration), pulses the
  `TerminalWaker`, and writes the cache. **Never on the loop; never a polling
  timeout** (perf invariant).
- Pane spawn does not wait: it applies the cache **if already warm**, otherwise
  the resolve kicks off and applies to subsequent spawns. The **active
  workspace is prewarmed at startup** so the first real pane is usually warm.

### Inject

- At pane spawn, merge the cached exported vars into the pane process
  environment **host-side, before the sandbox exec** — mirroring the existing
  resolve-at-spec-build pattern (`sandbox.rs:572`, `devenv_path`).
- `PATH` is **prepended** (dev tools win) but the user's existing `PATH` is
  preserved after it. Other exported vars are set if unset (don't clobber a var
  the user explicitly set in their shell).
- The cached store paths are already realized and bind-mounted read-only, so the
  tools execute unchanged in every sandbox backend.

### Guard

- Activates only when the repo root has a flake exposing a `devShell`. Non-nix
  repos are a clean no-op (no `nix` invocation, zero cost).
- Gated by config `[sandbox] inject_devshell` (default `true`).

## Tier B — opt-in daemon (fast-follow)

- New config key `[sandbox] nix_daemon` (default `false`).
- When `true`, the `SandboxSpec` bind-mounts `/nix/var/nix/daemon-socket/socket`
  (plus the `/nix/var/nix` paths the protocol requires) and sets
  `NIX_REMOTE=daemon`, so full `nix develop`/`build`/`fmt` work inside the
  sandbox.
- **Host precondition check:** if no daemon socket exists on the host, warn and
  leave it off rather than producing a sandbox where nix is half-wired.
- This is the only piece that relaxes isolation, hence opt-in and independent of
  Tier A.

## Contributor onboarding (scope 2)

- **`just doctor`** (new recipe): checks for `nix`, the devShell tools
  (`shellcheck`/`yamllint`/`taplo`/`treefmt`/`cargo-llvm-cov`), and a writable
  store; prints one actionable line per gap (e.g. "not in nix develop — run
  `nix develop`, or rely on thegn `[dev] inject_devshell`").
- **Committed `.envrc`** (`use flake`): a convenience for contributors who
  already use direnv. thegn never depends on it.
- **Friendlier `just` failures:** lint/fmt recipes detect a missing tool and
  print "run inside `nix develop`" instead of a bare `command not found`.

## Config summary

| Key                         | Default | Effect                                                             |
| --------------------------- | ------- | ------------------------------------------------------------------ |
| `[sandbox] inject_devshell` | `true`  | Tier A: resolve + inject the repo devShell env into worktree panes |
| `[sandbox] nix_daemon`      | `false` | Tier B: bind-mount the nix daemon socket into the sandbox          |

Both live under `[sandbox]` so the nix/sandbox tuning stays in one section.

Both documented in `config/config.toml.example`.

## Edge cases

- **Remote worktrees:** skip in v1 (resolving on the remote is out of scope);
  log that injection was skipped for a remote worktree.
- **Host store also read-only / no daemon** (the pathological all-sandboxed dev
  host): `print-dev-env` fails → resolver returns `None` → pane gets exactly
  today's `PATH`. **No regression**, just no improvement in that environment.
- **Non-flake / no devShell repo:** clean no-op.
- **flake eval errors:** treated as resolve failure (degrade + log), never fatal
  to pane spawn.

## Testing

- **Core unit tests** (`thegn-core`, 95% line gate): the `print-dev-env --json`
  output parser (exported-var extraction, malformed/empty input) and the
  cache-key derivation + invalidation logic. These are pure and do not shell out.
- **Subprocess seam:** the actual `nix print-dev-env` invocation sits behind the
  subprocess seam (excluded from coverage via the `cov_ignore` regex, per repo
  convention) and is exercised by smoke.
- **Smoke:** a flake repo yields a populated, cached env (with the tool bin dirs
  on the injected `PATH`); a non-flake repo is a clean no-op; `just doctor` exits
  non-zero with actionable output when a tool is missing and zero when present.
- **Perf:** assert resolution never runs on the loop (background-thread +
  waker), consistent with the existing render-decision invariants.

## Source map (where this lands)

- `crates/thegn-core/src/` — new `devenv.rs` (resolver + cache + parser);
  config keys in `config.rs` (`[sandbox] inject_devshell`, `[sandbox] nix_daemon`).
- `crates/thegn-core/src/sandbox.rs` — Tier B daemon-socket mount + env.
- `crates/thegn-host/src/` — pane-spawn env merge (the inject point near the
  existing sandbox spec build); background-resolve wiring + startup prewarm in
  `run.rs`.
- `justfile` — `doctor` recipe; tool-presence guards in `lint`/`fmt`.
- `.envrc`, `config/config.toml.example`, docs.

## Out of scope (YAGNI)

- direnv as a required mechanism.
- Resolving devShells for remote worktrees.
- Cache eviction beyond hash-based invalidation.
- Per-pane devShell selection (one devShell per repo for v1).
