## Context

Today superzej acquires exactly one external tool through code: the `pi` coding
agent. `crates/superzej-host/src/cmd/agent.rs` hard-codes the npm install
(`npm install --prefix <dir> @earendil-works/pi-coding-agent@<PI_PIN>`), a
`.superzej-pi-version` marker, an `is_current()` skip check, and a
`run_setup_cmd` helper that captures output when the TUI is active and inherits
stdio on the CLI. `PI_PIN` lives in `pi_assets.rs`.

`superzej-core` is deliberately substrate-agnostic: no tokio, no termwiz, and —
relevant here — **no HTTP client** (`reqwest` is only in `superzej-host`,
`superzej-svc`, `superzej-proxy`). Core already has the pieces the resolver's
pure logic needs: `util::which_path` / `util::have` (PATH lookup),
`util::superzej_dir()` / `util::managed_pi_dir()` (path roots), and the layered
TOML config in `config.rs`. Coverage is gated at 95% lines on core only;
I/O/subprocess seams are `cov_ignore`-excluded and exercised by `test/smoke.sh`.

This is Phase 1 of a four-phase effort modeled on Zed's per-extension
toolchain-acquisition pattern (user-override → PATH → download-and-pin, per
platform, with an update policy). Later phases (a BugStalker debug adapter,
user-declared MCP servers) are consumers of this resolver and are out of scope
here.

## Goals / Non-Goals

**Goals:**

- One reusable, pure, unit-tested resolver in core for "resolve + pin an external
  tool," generalizing the managed-pi logic.
- Keep core HTTP/tokio-free: core computes the _decision_ (which tier, which
  asset, whether a refresh is needed, what path); the host performs the fetch.
- Refactor pi onto the resolver with **zero observable change** to `szhost agent
setup`.
- Report resolution in `szhost doctor`.

**Non-Goals:**

- The BugStalker adapter, MCP-server specs, and capability-scoped grants (later
  phases).
- A general plugin/extension runtime — there is intentionally none.
- Auto-update on launch, checksum/signature verification, or mirror/air-gapped
  delivery (leave hooks in the update policy, but do not build them now).

## Decisions

### Split: pure `ManagedTool` in core, fetch in host

`superzej-core::managed_tool` holds the spec and all pure logic; the actual
download/npm-install lives in `superzej-host`.

- **Why:** core carries no HTTP client and must stay cross-compilable and 95%
  covered by pure unit tests. Mirrors the existing `github.rs` shape (domain
  model + parsing in core; the acting subprocess elsewhere) and the
  core-decides / svc-or-host-acts seam used across the codebase.
- **Alternative considered:** add `reqwest`/`ureq` to core and download there —
  rejected: it would drag an async/TLS stack into the substrate-agnostic crate
  and break the coverage model (network I/O can't be unit-tested in-crate).

### Core surface

```
enum Source { GithubRelease { repo: String, assets: Vec<AssetRule> }, Npm { package: String } }
struct AssetRule { os: Os, arch: Arch, asset: String }   // per-(os,arch) selector
enum UpdatePolicy { Always, Once, Never }
struct ManagedTool { name, source, version, policy, path_fallbacks: Vec<String> }

enum Resolution { Override { path, args }, OnPath { path }, Managed { path, current: bool } }

impl ManagedTool {
  fn asset_for(&self, os, arch) -> Option<&str>          // GithubRelease only
  fn managed_dir(&self) -> PathBuf                       // ~/.superzej/tools/<name> (pi keeps its legacy dir)
  fn bin_path(&self) -> PathBuf
  fn version_marker(&self) -> PathBuf
  fn is_current(&self) -> bool                           // marker == version
  fn needs_install(&self, force: bool) -> bool           // from policy + is_current + force
  fn resolve(&self, override_cfg: Option<&ToolOverride>, which: &dyn Fn(&str)->Option<String>) -> Resolution
}
```

`resolve()` takes the PATH lookup as an injected closure so the decision is pure
and testable (tests pass a fake `which`); production passes `util::which_path`.
`Os`/`Arch` are a small core enum with a `current()` constructor
(`cfg!(target_os/target_arch)`) — the one impurity, kept trivial and out of the
tested decision path (tests call `asset_for(os, arch)` with explicit values).

- **Why an injected `which`:** keeps tier-2 in the pure decision and avoids a
  real PATH dependency in unit tests.

### Config override shape

Add an optional, layered per-tool override read through existing `config.rs`
plumbing:

```toml
[tools.pi]        # or [tools.<name>]
path = "/usr/local/bin/pi"   # tier-1 override
args = ["--foo"]             # optional extra args
```

Absent config ⇒ `None` ⇒ resolver falls through to PATH/managed. This is the
`lsp.<server>.binary` analog from Zed, scoped to managed tools.

- **Alternative considered:** a dedicated env var per tool — rejected as less
  discoverable and inconsistent with superzej's layered-TOML convention.

### pi refactor keeps its exact paths and behavior

pi is expressed as `ManagedTool { name: "pi", source: Npm { "@earendil-works/pi-coding-agent" }, version: PI_PIN, policy: Once, path_fallbacks: ["pi"] }`.
To avoid churn and preserve resurrection/sprite compatibility, pi's
`managed_dir()`/`version_marker()` return the **current** locations
(`util::managed_pi_dir()`, `.superzej-pi-version`). `setup()` becomes: build the
spec → `needs_install(force)` gate (same as `!is_current()`) → host installer →
existing seed/register/marker steps unchanged. The npm-absent `pi`-on-PATH
fallback maps to tier-2. `agent.rs::is_current()` delegates to
`ManagedTool::is_current()`.

- **Why:** the change is a refactor, not a behavior change; smoke tests and
  sprite carry must keep passing byte-for-byte.

### Host installer

A small `managed_tool` host module (or a function in `cmd/agent.rs` reused by
others) with `install(tool, force) -> Result<()>`:

- `Npm` → `npm install --prefix <managed_dir> <package>@<version>` via the
  existing `run_setup_cmd`.
- `GithubRelease` → resolve the asset for `Os/Arch::current()`, download via
  `reqwest` (already a host dep) to `bin_path()`, `chmod +x`. (No GitHub-release
  consumer ships in this phase; the path exists for Phase 2 and gets a smoke
  test, not just dead code — pi exercises the `Npm` arm.)
- On success, write the version marker via the core helper.

## Risks / Trade-offs

- **[pi regression]** The refactor could subtly change `agent setup`. → Preserve
  exact paths/marker/seed order; gate behind `needs_install` that equals today's
  `!is_current()`; verify via `test/smoke.sh` and a manual `agent setup` /
  `agent path` check.
- **[unused GithubRelease arm]** Shipping the download path with no consumer
  risks bit-rot / lint (dead code). → Keep the pure `asset_for`/path logic fully
  unit-tested now; keep the host download arm minimal and covered by a smoke
  step; Phase 2 (BugStalker) is its first real consumer immediately after.
- **[coverage]** New core logic must clear the 95% gate. → The resolver is pure
  and fully unit-testable (tiers, asset selection, marker/policy, path
  computation); the fetch is `cov_ignore` + smoke.
- **[platform detection]** `Os/Arch::current()` is the one impure spot. → Keep it
  a thin `cfg!` mapper; all decision tests take explicit `(os, arch)`.

## Migration Plan

Pure addition + internal refactor; no schema, no config-breaking change (the
`[tools.*]` block is optional and additive). Rollback = revert the change; pi's
on-disk layout and marker are unchanged, so a rolled-back binary keeps working
against an already-installed managed pi.

## Open Questions

- Should the managed tools root be `~/.superzej/tools/<name>` for new tools while
  pi stays at its legacy `~/.superzej/pi`? (Proposed: yes — new tools get the
  namespaced dir; pi keeps its path for compatibility.)
- Later-phase concern only: whether GitHub-release downloads should verify a
  checksum. Deferred; the update policy leaves room to add it.
