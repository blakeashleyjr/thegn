# THE-60 — Full mise support

## Decision

Add a generic, substrate-free toolchain-activation seam in `thegn-core`, then
adapt the existing Nix devshell and direnv paths and add one host-side mise
provider to it. Detection and merge policy stay pure. Only
`crates/thegn-host/src/mise_provider.rs` may construct or execute a `mise`
process. A worktree launch consumes a cached activation plan; a cold or
unavailable provider degrades to the base shell and reports why.

The install operation is explicit. It is available as a palette action named
`toolchain-install`, runs off the event loop, and refuses an unapproved repo
config. Normal pane/agent creation never performs a network install. Existing
provisioning may retain an explicit, off-loop provisioning
step, but it must use the provider implementation and must never silently
`trust` a repo or turn a pane spawn into an install loop.

No SQLite migration is needed. The existing repo-trust approval table gates the
`mise.env` config-set identity; the resolved environment is a 0600 state-dir
cache, never config, DB, log, or notification content.

## Verified branch state and draft verification

The branch was checked against `CLAUDE.md`, `docs/ARCHITECTURE.md`, and every
file in `openspec/changes/add-mise-toolchain-provider/` before defining the
chunks. The relevant current facts are:

| Current fact                                                | Evidence and consequence                                                                                                                                                                                                          |
| ----------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Core detection is pure and local/remote are coupled         | `crates/thegn-core/src/envplan.rs:94-99`; the detector and `DETECT_PROBE_SCRIPT` must be extended together.                                                                                                                       |
| Detection is incomplete                                     | `crates/thegn-core/src/envplan.rs:100-115,148-211` only checks the first four mise-ish files and `.nvmrc`; the full config/pin set belongs in one normalized `DetectedToolchainFiles` value.                                      |
| Nix currently outranks tool versions                        | `crates/thegn-core/src/envplan.rs:61-73`; retain Nix provisioning precedence while allowing mise activation as a lower layer.                                                                                                     |
| The current toolchain config is flat                        | `crates/thegn-core/src/toolchain.rs:17-50`; add a nested `MiseConfig` without disturbing existing `mode` and Nix package overrides.                                                                                               |
| Nix already has the right off-loop/cache shape              | `crates/thegn-core/src/devenv.rs:1-17,135-203`; reuse its lifecycle contract, but put the generic merge policy in a new module.                                                                                                   |
| Bundle composition is already a launch seam                 | `crates/thegn-core/src/bundle.rs:52-72,296-309`; its `is_credential_key` filter at `:703-706` is the single filter mise must call.                                                                                                |
| Spawn composition is centralized                            | `crates/thegn-host/src/agent.rs:3094-3125,3168-3203,3220-3297`; inject the activation plan here so ordinary panes and agents share it.                                                                                            |
| Daemon agents use the same path                             | `crates/thegn-host/src/daemon/agent_open.rs:17-27,92-113`; no second daemon-only mise implementation is allowed.                                                                                                                  |
| Current mise provisioning is unsafe and in the wrong module | `crates/thegn-host/src/host_provision.rs:1276-1284` curls a binary, unconditionally trusts, and installs. Move that operation into the provider and make trust/install explicit.                                                  |
| Named execution environments are selected before launch     | `crates/thegn-host/src/agent.rs:3104-3125`; the selected `[env.<name>]` changes placement/sandbox, not toolchain policy.                                                                                                          |
| `wt new --env` persists a named environment selection       | `crates/thegn-host/src/cmd/wt.rs:79-93,191-201`; the effective worktree/workspace fallback is `crates/thegn-core/src/db_workspace.rs:745-748`. The provider must consume that resolved selection, not invent a parallel selector. |
| Existing env token/chip seams are cheap to extend           | `crates/thegn-host/src/tabbar_env.rs:1-31`; `PanelData.environments` is hydrated off-loop at `crates/thegn-host/src/panel/mod.rs:694-703`.                                                                                        |
| Help/action and catalog ratchets are active                 | `docs/ARCHITECTURE.md:151-177,199-214,216-228`; `CLAUDE.md:304-315,357-377`. A UI action needs the action registry/help ratchets; no control verb is needed.                                                                      |

The OpenSpec draft is useful but not authoritative. Its detection, cache,
precedence, trust, doctor, and no-DB-migration claims are retained from
`proposal.md:38-108`, `design.md:3-98`, `tasks.md:3-55`, and
`specs/sandbox/spec.md:5-108`, subject to the branch evidence above. The
following draft claims are deliberately changed:

1. `design.md:112-116` defers a full provider seam until a second manager.
   Cut: THE-60's binding addendum requires the generic seam now, and the
   architecture requires provider registration rather than a mise-shaped
   special case.
2. `design.md:96-98` says there is no interactive surface. Cut: the issue
   explicitly requires an explicit install action, so add one palette action
   with the normal help/action ratchets.
3. `proposal.md:62-69` and `design.md:72-79` allow implicit install during
   provisioning. Narrow: a deliberate provisioning command may run a provider
   step off-loop, but pane/agent activation never downloads, and the user-facing
   install retry is explicit and trust-gated.
4. The draft's `mise.env` approval cannot be listed/approved by the current
   `repo trust` command unless its pending request is added to that command's
   existing `config_resolve` list. Chunk 2 wires both listing and approval; no
   parallel trust database is introduced.

## Core model

Create `crates/thegn-core/src/toolchain_activation.rs`; do not grow
`envplan.rs`, `toolchain.rs`, or `bundle.rs` into a second god module. It owns
only values and pure functions:

- `DetectedToolchainFiles`: deterministic, relative paths split into config
  files and pin files, including `mise.toml`, `.mise.toml`, `mise.local.toml`,
  `mise/config.toml`, `.mise/config.toml`, `.config/mise.toml`,
  `.config/mise/config.toml`, safe `conf.d/*.toml`, `MISE_ENV`'s
  `mise.<env>.toml`, `.tool-versions`, `.nvmrc`, `.node-version`,
  `.python-version`, `.ruby-version`, `.go-version`, and `.java-version`.
  Ignore path traversal, symlinks outside the worktree, unreadable entries,
  and malformed probe lines. The local detector and remote probe emit the same
  normalized relative names; no config contents are sent in the probe.
- `ToolchainProvider` as an object-safe, synchronous policy seam over explicit
  input/output values. It has a stable `kind`, a `probe` answer, and an
  activation answer. Its answer is `Ready`, `Unavailable`, or `Reserved`;
  `Reserved` means the provider cannot answer this context and is a normal
  no-op, not an error. No async trait and no process, terminal, tokio, HTTP, or
  vendor type enters core.
- `ActivationLayer` containing ordered PATH entries, safe env pairs, provider
  origin, and status. `ActivationPlan` contains the final ordered layers and
  reportable statuses. A path entry is data, never a shell command.
- Pure `compose_activation(bundle, devshell, provider_answers, base_path)`:
  explicit bundle PATH first, Nix/devshell PATH next, mise PATH/shims next,
  base PATH last. Bundle values win; devshell values fill only unset values;
  mise `[env]` values fill only remaining gaps and call
  `bundle::is_credential_key` before entering the plan. `PATH` from mise's
  `_.path` is inserted only at the mise slot. Stable key/path ordering is
  unit-tested.
- Pure cache/trust identity: hash the worktree identity plus every detected
  config/pin file's relative name and bytes, including `mise.lock` when
  present. `mise.env` is a `GatedRequest` whose value contains only the stable
  config-set hash and file names. An edit therefore produces a new request and
  a new cache key. No values, credentials, or command output are part of the
  request summary.

`envplan::EnvRequirements` gets the normalized detection result while retaining
`tool_versions` as a compatibility boolean. `tier()` stays Nix > ToolVersions

> SynthNix > Languages > Bare. A Nix flake/devenv/classic environment is still
> the provisioning owner when it coexists with mise; `mode = "mise"` still
> selects the mise provisioning tier for a languages-only repo.

`MiseConfig` is nested under `ToolchainConfig`:

```toml
[toolchain.mise]
inject = "auto" # auto | shims | env | off
```

`auto` applies shims immediately and applies the filtered mise environment only
after the repo config-set is approved. `shims` applies only the shims path.
`env` uses the same shims fallback while its cached environment is pending;
`off` answers `Reserved` and changes nothing. There is no `[toolchain.mise.env]`
override: the process's `MISE_ENV` is detected as part of the worktree context,
and ambient env selection must not become a hidden second config layer.

The key is intentionally pinned in `test/env-overlay-ratchet.txt` beside
`toolchain.mode`: activation/trust policy is not an ambient CI override. The
config example documents this rationale and the generated config-reference
page picks it up.

## Host provider and launch flow

Add `crates/thegn-host/src/mise_provider.rs`. This is the only source file that
may contain a `mise` executable name or invoke it. It implements the core
provider and owns:

1. read-only binary/version and trust-state probes for doctor/status;
2. shims directory discovery (`MISE_DATA_DIR`, then the provider's documented
   default), without executing repo code;
3. approved, bounded `mise env -s json` resolution in the worktree, off-loop;
4. missing-tool inspection used for doctor/status; and
5. the explicit `mise install`/provision command for the selected target.

Every child process has a bounded timeout, a sanitized environment, no output
in logs, and a failure result that becomes `Unavailable`/`Degraded`. The
provider never uses shell interpolation for the worktree path. The install path
checks the `mise.env` approval first; it never performs `mise trust` for an
unapproved config. Host mise is never auto-installed. If a sandbox/remote target
does not have mise, the provider reports that fact rather than downloading a
host binary.

The host adds one adapter each for the current Nix devshell and direnv paths:
Nix converts the cached `devenv::Devshell` into a ready/reserved activation
answer; direnv reports its existing warm/trust state and remains the shell-hook
owner, rather than being evaluated a second time by this seam. No new direnv or
Nix subprocess path is created. All three provider answers pass through the
same core composer, which is where `Reserved` and precedence are made visible.

In `agent::launch_spec_full`, after the existing bundle resolution and before
the final `compose_spec`, read only the cached activation answer. If warm, fold
the plan into `SandboxSpec.env_overrides`/`init_script` or host `LaunchSpec.env`;
if cold, use shims (if safe) and schedule provider resolution. The resolver
uses the existing refresh channel and `TerminalWaker`; it never waits on the
event loop. Preserve the current bundle/devshell behavior while replacing the
two ad-hoc PATH injectors with one plan application function. This keeps
`daemon/agent_open.rs` and every TUI worktree/shell launch on the same path.

For `[env.<name>]`, resolve the selected environment first as today. The
provider target is then the resolved local/sandbox/remote placement. A remote
worktree uses the existing remote detection/exec seam; if the target cannot
answer, it returns `Reserved` and the pane still opens. Do not inspect a local
host path for a remote worktree and do not infer the selected env from the
active global default.

## Trust, doctor, status, and explicit install

Extend the existing repo-trust flow, whose canonical requests are
`{"key":...,"value":...}` (`crates/thegn-core/src/config_resolve.rs:141-165`):

- the core creates the `mise.env` request from the config-set identity;
- `handlers/repo_trust.rs` surfaces it through the existing deduplicated
  notification path;
- `cmd/repos.rs` lists it and approves/revokes it with the same command and
  DB table as sandbox overlay requests; and
- only an approved request permits host `mise env` resolution or explicit
  install. An edit re-prompts. Shims alone require no approval because a PATH
  prepend is not repo execution.

Doctor adds a worktree-context toolchain report beside the existing provider
reports (`crates/thegn-host/src/cmd/doctor.rs:307-330`): binary/version,
detected files, tier, inject mode, shims state, env state, trust state, missing
tool names/count, and the exact degradation reason. The report is presence-only
and never prints resolved env values. A missing binary is an unavailable
provider, not a doctor failure.

Hydration reads the cached status off-loop into `PanelData`. The environment
panel shows a `WORKTREE TOOLCHAIN` line with provider/tier, trust/pending, and
missing-tool state. `tabbar_env` adds a terse `(mise)`, `(mise ~)`, or
`(mise !)` token to the existing env cluster; it is omitted for no declaration
or `off`. Token rendering uses the existing glyph/color chokepoints and does
not add a new frame-time probe. Selecting `toolchain-install` starts the
provider's bounded install task, pulses the waker, and refreshes the cached
status; it does not block or auto-retry.

There is deliberately no new control `Verb`, HTTP/gRPC route, MCP tool, or
plugin host call. Install is a local, user-confirmed host action like the
existing environment wizard actions, so `CATALOG`, `API_CALLS`, and
`docs/api/control-v1.json` must remain byte-for-byte unchanged. The coder must
run the control-schema test in the same chunk and treat any diff as a design
error, not regenerate a snapshot for an unrequested wire surface. Likewise,
the install action takes no value, so it adds no completion-slot entry.

## Ratchets and verification

- Core chunk updates `test/env-overlay-ratchet.txt` with
  `toolchain.mise.inject`, and runs the env-overlay coverage test. It does not
  create an env knob for the trust policy.
- UI chunk adds `toolchain-install` to `Action`, `ACTION_SPECS`, and a real
  authored help page; it regenerates/validates the three applicable help
  ratchets. No new panel context is introduced, so
  `test/help-context-ratchet.txt` stays unchanged.
- The UI chunk runs completion-slot coverage and the control-schema snapshot
  test; both are intentionally unchanged because there is no value-taking
  install argument and no control wire type/route.
- Core tests cover full local/remote detection parity, `MISE_ENV` and lock
  invalidation, `Reserved`, trust re-prompt, PATH/env precedence, credential
  filtering, and mode behavior. Host tests use fake command runners/cache
  files; no real mise, network, or live DB is required.
- No e2e is part of this design pass. Any manual `thegn` invocation from this
  worktree must set `XDG_STATE_HOME` to a fresh temporary directory. Do not
  run `just test`, `just ci`, or a full-workspace build while implementing the
  chunks; use the scoped commands in the chunk specs.
