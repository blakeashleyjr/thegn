## 1. Core: managed_tool module

- [x] 1.1 Add `crates/thegn-core/src/managed_tool.rs` and export it from `lib.rs`.
- [x] 1.2 Define `Os`/`Arch` core enums with a `current()` `cfg!`-based constructor and a display form.
- [x] 1.3 Define `Source` (`GithubRelease { repo, assets: Vec<AssetRule> }` / `Npm { package }`), `AssetRule { os, arch, asset }`, `UpdatePolicy` (`Always`/`Once`/`Never`), and `ManagedTool { name, source, version, policy, path_fallbacks }`.
- [x] 1.4 Implement `asset_for(os, arch) -> Option<&str>` for GitHub-release sources (unsupported platform ⇒ `None`).
- [x] 1.5 Implement `managed_dir()`/`bin_path()`/`version_marker()` (namespaced `~/.thegn/tools/<name>` for new tools; a pi `with_layout` override keeps the legacy `util::managed_pi_dir()` + `.thegn-pi-version`).
- [x] 1.6 Implement `is_current()`/`is_current_at(dir)` (marker == version && bin exists) and `needs_install(force)` from policy + `is_current` + force (pure `should_install`).
- [x] 1.7 Implement `Resolution` enum and pure `resolve(override_cfg, which_fn)` following the fixed tier order (override → PATH via injected closure → managed).

## 2. Core: config override

- [x] 2.1 Add an optional layered `[managed_tools.<name>]` override (`path`, optional `args`) to `config.rs` as a `BTreeMap<String, ToolOverride>` field (matching env/bundle), read through existing layered-config plumbing; absent ⇒ empty. (Named `managed_tools`, not `tools`, to avoid colliding with the existing `[[tools]]` picker array.)
- [x] 2.2 `ToolOverride` type lives in `managed_tool.rs` and is consumed directly by `resolve()` and `doctor`.

## 3. Core: unit tests (95% gate)

- [x] 3.1 Test the three-tier order: override wins; PATH fallback before download; managed as last resort (fake `which` closure).
- [x] 3.2 Test `asset_for` per `(os, arch)` including an unsupported platform, plus `Os/Arch::current()`.
- [x] 3.3 Test `is_current`/`should_install` across policies and a version bump, the deterministic path computation, and `ToolOverride` TOML round-trip.

## 4. Host: fetch seam

- [x] 4.1 Add a host `managed_tool` module with `acquire(tool)`: `Npm` → `npm install --prefix <managed_dir> <package>@<version>` via the shared `run_setup_cmd`; `GithubRelease` → download `asset_for(current)` via `reqwest::blocking`, write to `bin_path()`, `chmod +x`. (Generic `install(tool, force)` = gate+acquire+mark deferred to Phase 2, its first consumer, to avoid dead code.)
- [x] 4.2 `mark_installed(tool)` writes the version marker best-effort; the fetch stays off the event loop (CLI / `spawn_blocking`) and surfaces failures.

## 5. Host: refactor managed-pi onto the resolver

- [x] 5.1 Express pi as a `ManagedTool` (`pi_tool()`: `Npm` `@earendil-works/pi-coding-agent`, `version = PI_PIN`, `policy = Once`, fallback `["pi"]`) keeping its legacy dir + marker via `with_layout`.
- [x] 5.2 Rewrite `cmd/agent.rs::setup` to gate on `needs_install(force)` and call `acquire`; seed/register/messages unchanged; marker written last (after register) via `mark_installed` so `is_current()` still means "fully set up".
- [x] 5.3 `agent.rs::{is_current, managed_pi_bin}` delegate to `pi_tool()`; the npm-absent `pi`-on-PATH fallback is preserved (register + tier-2).

## 6. Host: doctor reporting

- [x] 6.1 Add a managed-tools section to `cmd/doctor.rs`: per tool, the resolved tier, path, and pinned-vs-installed version state; plus the `--json` `managed_tools` array.

## 7. Verification

- [x] 7.1 `cargo test -p thegn-core` green (1132 tests incl. 9 managed_tool). NOTE: `just coverage` (llvm-cov, nix) not run in this environment; all pure resolver logic is unit-tested, the fetch is a `cov_ignore`/smoke seam.
- [x] 7.2 `cargo clippy -p thegn-core -p thegn-host --all-targets` clean; `cargo fmt --check` clean; god-file ratchet OK (config.rs 9879 → 9863 via moving `impl ForwardConfig` to `forward.rs`; new code in new sibling modules).
- [x] 7.3 Manual `thegn agent path` (legacy pi layout unchanged) + `thegn doctor` / `--json` (pi listed; resolved via the PATH tier on this machine) confirm behavior. NOTE: full `test/smoke.sh` (which exercises the real `agent setup` npm fetch) not run here; the acquire path is unchanged from the prior pi install.
