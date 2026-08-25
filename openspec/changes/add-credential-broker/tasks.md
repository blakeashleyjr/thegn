# Tasks — add-credential-broker

Iterate with `just quick <crate>` and targeted `cargo nextest run -p <crate>
<substring>`; the full gates run once at the end (dev-loop policy).

## 1. Typed SecretRef (pure core)

- [ ] 1.1 `thegn-core/src/secretref.rs`: `SecretRef` enum
      (Keyring/Env/File/Literal), `parse(s, BareAs)` with
      `BareAs::{EnvName, Literal}`; redacted `Debug`, no `Display`/serialize
      of literal values. Unit tests incl. the redaction sentinel test
      (95% core gate).
- [ ] 1.2 Parse-at-load accessors on config structs for every secret field
      (`api_key_env` family, `[[issues.accounts]].token`, `[ci.*].token`, VPN
      auth keys, MCP upstream env values); config schema stays `String` — no
      format change, no home-manager churn.
- [ ] 1.3 `thegn config validate`: warning for `Literal` refs naming the
      field + fix; unit tests.

## 2. Broker seam + backends

- [ ] 2.1 `SecretStore` seam (coordinate with `add-mcp-proxy-hub` 4.1 — one
      trait, first-lander builds it): object-safe get/set/del/list under the
      thegn service namespace, seam-classed errors, `kind`
      implemented-or-reserved (`keyring`, `file`, `env`; `exec` reserved).
- [ ] 2.2 Rehome `thegn-host/src/secret.rs` as the keyring+file backend impl
      (bounded probe, presence memo, writer path preserved); `env` backend;
      broker chokepoint fn used by all host callers.
- [ ] 2.3 Migrate consumers to injected resolution: `thegn-svc/src/issue/`
      (drop `expand_env_ref` for tokens), CI clients, VPN keys, snapshot
      store (already injected — same shape), iroh key naming.
- [ ] 2.4 Migrate the Kaneo device-flow token out of the state DB into the
      store (read-through fallback for one release; no schema bump — row
      simply stops being written).
- [ ] 2.5 Doctor: Secrets section — backend probe rows + per-ref
      presence lines (via the presence memo); `exec` shown reserved.
- [ ] 2.6 `thegn secret migrate`: literal → store + `config_write` rewrite;
      smoke-tested (I/O path, cov_ignore).

## 3. CLI + catalog

- [ ] 3.1 `thegn secret set|rm|list|migrate|audit` + `thegn secret ssh
rotate` (`cmd/secret.rs`); `list`/`audit` are names+backends only — no
      value-read verb exists.
- [ ] 3.2 CATALOG rows for each verb, `SurfaceSet::OPERATOR`,
      `required_scope` wiring; extend the pinned surface-set test.
- [ ] 3.3 `thegn mcp secret *` (if landed) delegates to the same store
      namespace — no second store.

## 4. Audit trail

- [ ] 4.1 Audit event type + `thegn::secret::audit` tracing at the
      chokepoint (ref, backend, consumer tag, outcome); consumer tags for
      provider/issues/ci/snapshot/mcp/agent_task callers.
- [ ] 4.2 Redaction unit test (sentinel bytes absent from all renderings).
- [ ] 4.3 Optional JSONL sink `[credentials] audit_file` (default false),
      metadata only, under the state dir.

## 5. SSH identity custody

- [ ] 5.1 `[credentials.ssh] managed_key_scope = "shared"|"per-account"`
      (default shared); per-account keypath naming; provisioning paths
      (provider_factory, vps, machine0) select the scoped key for new
      instances.
- [ ] 5.2 `thegn secret ssh rotate [--account]`: generate → authorize
      everywhere in scope → verify connect → de-authorize old → retire;
      partial-failure reporting keeps both keys live. Smoke/e2e-adjacent
      test against a fake transport.
- [ ] 5.3 Destroy paths record the authorized managed key (audit event).

## 6. Host-key policy table

- [ ] 6.1 `thegn-core/src/hostkey.rs`: connection-class enum + policy table +
      argv-builder chokepoint (pure, unit-tested).
- [ ] 6.2 Migrate call sites: `vps/ssh_shim.rs`, `host/mod.rs` (iroh alias
      pin), `envplan.rs` bootstrap, `agent_ssh.rs`/`agent.rs`
      (LoopbackTunneled), remote.rs `ssh_base`.
- [ ] 6.3 New shrink-only ratchet `test/hostkey-ratchet.txt` wired into
      `just lint` (no host-key literals outside the chokepoint); seed with
      any deliberately-deferred sites.
- [ ] 6.4 Doctor: class → policy → justification table.

## 7. Exposure policy

- [ ] 7.1 Sealed/SealedTunnel default clamp: drop `SSH_AUTH_SOCK`
      passthrough + `/run/user` mount for those tiers (explicit config
      re-adds; doctor flags it); tests on the env-assembly fold.
- [ ] 7.2 Force no-agent-forwarding (`-a`) for ManagedFresh +
      LoopbackTunneled argv builders; user-declared hosts unchanged.
- [ ] 7.3 Doctor per-tier secret-exposure listing (env vars, sockets,
      mounts).

## 8. Signing

- [ ] 8.1 `[identities.<name>.signing]` (`format`, `key`) on
      `IdentityConfig` (additive to add-decoupled-identities); resolution
      into `gpg.format`/`user.signingKey` at the existing compose/spawn fold;
      unit tests for scope-chain override.
- [ ] 8.2 Verify interplay with `[git] override_gpg` and the commit-overlay
      cycle (operation layer wins); document fold-commit signing behavior in
      the merge-queue docs.

## 9. Docs + config surface

- [ ] 9.1 `config/config.toml.example`: `[credentials]`, `[credentials.ssh]`,
      `[identities.<name>.signing]` keys documented; JSON-schema tests.
- [ ] 9.2 `docs/help/` — no new actions/keybinds (no help-ratchet claims
      needed); extend the profiles/identities help prose where signing is
      user-visible; doctor output documented.
- [ ] 9.3 Update `docs/extending/provider-impl.md` cross-reference for the
      `SecretStore` backend recipe.

## 10. Gate

- [ ] 10.1 Run `just ci` once (includes openspec validate, lint ratchets,
      coverage on the new core modules).
