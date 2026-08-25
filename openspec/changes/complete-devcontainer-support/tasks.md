# Tasks — complete devcontainer support

## 1. Parser honesty (thegn-core, pure)

- [ ] 1.1 `devcontainer.rs`: add `recognized_unapplied()` — parse-time
      inventory classifying every containers.dev key as
      applied / refused / reserved / editor-only; exhaustive test that every
      reference key lands in exactly one class (a new field fails until
      classified).
- [ ] 1.2 Multi-config discovery: extend `detect_and_parse` to
      `.devcontainer/<folder>/devcontainer.json`; return all candidates.
- [ ] 1.3 `${devcontainerId}` substitution: stable
      `short_hash(repo_root + config_relpath)`; unit test stability and
      per-variant distinctness.
- [ ] 1.4 Clamp `${localEnv:VAR}`: substitution reports which vars it looked
      up, or takes an allowlist predicate; unit-test that a non-allowlisted
      var resolves empty and is reported.
- [ ] 1.5 Unit tests for parse-failure surfacing (malformed JSONC ⇒ error
      string, no partial result).

## 2. Overlay honesty (thegn-core, pure)

- [ ] 2.1 `devcontainer_overlay.rs`: emit refusal warnings for
      `privileged`/`capAdd`/`securityOpt`/`runArgs`/`init` (never applied,
      even with approvals present) and reserved-key warnings from the 1.1
      inventory; drop the stale `unsupported()` doc reference.
- [ ] 2.2 Backend check: when the folded source is a container shape and the
      effective backend family is not OCI, produce a warning naming the
      backend; unit test per backend family.
- [ ] 2.3 Precedence tests: user-pinned `image`/`profile`/`backend`/`network`
      survive the fold; additive lists append.
- [ ] 2.4 `devcontainer_features.rs`: honour `installsAfter`/`dependsOn` from
      fetched metadata when present (topological, cycles fall back to
      declaration order with a warning); keep
      `overrideFeatureInstallOrder`-first; unit tests for order + cycle
      fallback.

## 3. Config (thegn-core)

- [ ] 3.1 `config_sandbox.rs`: `devcontainer = "auto" | "off"` via
      `config_enum!`; repo-overlay classification: preference (it only
      narrows repo-file usage); `.thegn.toml` `devcontainer = "<folder>"`
      selector key.
- [ ] 3.2 Document both keys in `config/config.toml.example` (the generated
      config-reference help page picks them up).
- [ ] 3.3 `config_enum!` round-trip + overlay clamp tests.

## 4. Host wiring (thegn-host)

- [ ] 4.1 `handlers/repo_trust.rs`: route the `env_passthrough` allowlist into
      the `SubstCtx` `local_env` closure; surface the localEnv warnings;
      plumb the multi-config selector; ambiguity ⇒ warn + none.
- [ ] 4.2 Respect `[sandbox] devcontainer = "off"` before parse (one notice).
- [ ] 4.3 `cmd/doctor.rs`: devcontainer section — presence/selected variant,
      parse result, per-category trust state, refused/reserved keys, backend
      honour-ability. No new catalog row (rides `doctor`).
- [ ] 4.4 Smoke-test the doctor block (`test/smoke.sh` covers the seam; core
      inventory logic is unit-covered).

## 5. Docs + spec

- [ ] 5.1 Update the sandbox help/docs pages that mention devcontainers with
      the classification table (config-reference page is generated — do not
      hand-write).
- [ ] 5.2 Verify the delta spec matches implemented behaviour; adjust
      scenarios if implementation details shifted.

## 6. Gate

- [ ] 6.1 Run `just ci` once (includes openspec-validate) when the
      implementation is complete.
