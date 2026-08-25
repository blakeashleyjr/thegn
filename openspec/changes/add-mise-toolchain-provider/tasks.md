# Tasks — mise toolchain provider

## 1. Detection (thegn-core, pure)

- [ ] 1.1 `envplan.rs`: widen `detect()` to mise's project-level config chain
      (`mise.local.toml`, `mise/config.toml`, `.mise/config.toml`,
      `.config/mise.toml`, `.config/mise/config.toml`, `conf.d/*.toml`,
      `MISE_ENV` variants) + idiomatic pin files (`.node-version`,
      `.python-version`, `.ruby-version`, `.go-version`, `.java-version`).
- [ ] 1.2 Extend `DETECT_PROBE_SCRIPT` with the same set (the contract says
      extend both together); update `detect_from_probe` tests.
- [ ] 1.3 Expose the detected config-file list (not just a bool) for cache
      keys and trust canonicalization; unit tests.

## 2. Config + pure merge logic (thegn-core)

- [ ] 2.1 `toolchain.rs`: `[toolchain.mise]` table —
      `inject = "auto"|"shims"|"env"|"off"` via `config_enum!` (default
      `auto`); document in `config/config.toml.example` (generated
      config-reference help page picks it up).
- [ ] 2.2 Pure merge: mise env layer folds at the pane compose seam — fills
      unset gaps only, credential-like keys
      (`*_TOKEN`/`*_KEY`/`*_SECRET`/`*_PASSWORD`) dropped, `_.path` entries
      join PATH at the mise slot; exhaustive unit tests (95% core gate).
- [ ] 2.3 PATH slot ordering: bundle > devshell inject > mise shims > base;
      lock with unit tests at the compose seam.
- [ ] 2.4 Cache-key derivation: content hash over the detected config set
      (include `mise.lock` when present); unit tests for
      invalidation-on-edit.
- [ ] 2.5 Trust canonicalization: the `mise.env` `GatedRequest` canonical form
      over the config set hash; re-prompt on edit; unit tests beside the
      existing `config_resolve` gating tests.

## 3. Host resolver + wiring (thegn-host)

- [ ] 3.1 New `mise_inject.rs` (sibling of the devshell-inject path): shims
      dir discovery (`MISE_DATA_DIR` respected), off-loop `mise env -s json`
      resolve on a `Utility`-QoS thread, state-dir cache write, refresh-channel
      send + `TerminalWaker` pulse; cold cache never blocks spawn.
- [ ] 3.2 `handlers/repo_trust.rs`: plumb the `mise.env` gated category
      through the same pending/approval surfacing as `.thegn.toml` and
      devcontainer requests.
- [ ] 3.3 Provisioning alignment: `mise_install_script` runs
      `mise trust <file>` only for thegn-approved configs (drop the
      unconditional `mise trust`).
- [ ] 3.4 `cmd/doctor.rs`: mise probe — binary/version, detected configs,
      inject mode, trust state, degradation reason. No new catalog row.
- [ ] 3.5 Smoke coverage for the resolver seam (`test/smoke.sh`); core logic
      stays unit-covered.

## 4. Docs + spec

- [ ] 4.1 Update the sandbox/toolchain docs pages for the injection modes and
      precedence table (never hand-write the generated config-reference
      page).
- [ ] 4.2 Verify delta spec scenarios against the implementation.

## 5. Gate

- [ ] 5.1 Run `just ci` once (includes openspec-validate) when the
      implementation is complete.
