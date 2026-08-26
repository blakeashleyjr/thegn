# Tasks — add-credential-broker

Iterate with `just quick <crate>` and targeted `cargo nextest run -p <crate>
<substring>`; the full gates run once at the end (dev-loop policy).

## 1. Typed SecretRef (pure core)

- [x] 1.1 `thegn-core/src/secretref.rs`: `SecretRef` enum
      (Keyring/Env/File/Literal), `parse(s, BareAs)` with
      `BareAs::{EnvName, Literal}`; redacted `Debug`, no `Display`/serialize
      of literal values. Unit tests incl. the redaction sentinel test
      (95% core gate).
- [~] 1.2 Parse-at-load: `thegn-core/src/secret_scan.rs` enumerates every
  configured secret field into typed `SecretRef`s with the right per-field
  `BareAs` (provider `api_key_env` = EnvName; issue/CI tokens = Literal).
  Config schema stays `String`. VPN keys / snapshot creds / MCP upstream env
  NOT yet added to the scan (deferred).
- [x] 1.3 `thegn config validate`: warning for `Literal` refs naming the
      field + fix (`cmd/config.rs`, advisory/non-failing); `secret_scan` unit
      tests cover the literal detection.

## 2. Broker seam + backends

- [x] 2.1 `SecretStore` seam (`thegn-core/src/secret_store.rs`): object-safe
      get/set/del/list, seam-classed `SecretError`
      (unavailable/denied/not-found), `kind` implemented-or-reserved
      (`keyring`, `file`, `env`; `exec` reserved). (First-lander built it.)
- [x] 2.2 Rehome `thegn-host/src/secret.rs`: `KeyringStore`/`FileStore`/
      `EnvStore` impls (bounded probe + presence memo + writer preserved);
      broker chokepoint `resolve_ref_for` used by `resolve`/`resolve_for`.
- [ ] 2.3 Migrate svc consumers to injected resolution (issue/CI/VPN drop
      `expand_env_ref`) — DEFERRED. The typed `SecretRef` + `secret_scan` land
      the vocabulary; the svc resolver-injection wiring is follow-up. Provider
      tokens do go through the broker (`resolve_for` with `provider:*` tags).
- [x] 2.4 Kaneo device-flow token moved OUT of the state DB into the broker:
      `thegn kaneo login` stores the raw token via the broker (0600 file) and
      records only a `file:` SecretRef in `kaneo_auth` (`persist_kaneo_token`);
      the svc read path resolves it via `expand_env_ref` with a read-through
      fallback for legacy raw-token rows. Tests assert no raw token lands in the
      DB row (and a store that echoes the raw value is rejected).
- [x] 2.5 Doctor: Secrets section — backend probe rows (`secret::probes()`) +
      per-ref presence lines (via `secret::present`); `exec` shown reserved.
- [x] 2.6 `thegn secret migrate`: literal → store + `config_write` rewrite
      (`set_issue_account_token` / `set_key`), `--dry-run`.

## 3. CLI + catalog

- [x] 3.1 `thegn secret set|rm|list|migrate|audit` + `thegn secret ssh rotate`
      (`cmd/secret.rs`); `set` reads stdin (never argv); `list`/`audit` are
      names+backends+presence only — no value-read verb. (`ssh rotate` reports
      the plan/scope; live per-instance authorize is scaffolded — see 5.2.)
- [x] 3.2 CATALOG rows for each verb (`secret.set/rm/list/migrate/audit/
ssh.rotate`), `SurfaceSet::OPERATOR`, Admin `required_scope`; Http/Grpc
      `SURFACE_GAPS`, CLI coverage in `cli_control_caps`; admin-caps test green.
- [ ] 3.3 `thegn mcp secret *` delegation — N/A until `add-mcp-proxy-hub`
      lands its verbs; the shared store is ready for it.

## 4. Audit trail

- [x] 4.1 Audit event type (`thegn-core/src/secret_audit.rs`) +
      `thegn::secret::audit` tracing at the chokepoint (ref name, backend,
      consumer tag, outcome). Provider callers pass `provider:*`; issue/ci/
      snapshot/agent_task tags exist via `secret_scan` but not every legacy
      caller passes one yet (they go through the generic `host` tag).
- [x] 4.2 Redaction sentinel tests (secretref + secret_audit: sentinel absent
      from Debug / serde / audit_name).
- [ ] 4.3 Optional JSONL sink — config field `[credentials] audit_file` added
      (default false); the sink writer itself is DEFERRED.

## 5. SSH identity custody

- [~] 5.1 `[credentials.ssh] managed_key_scope` (config + `config_enum`,
  default **per-account** per the approved tightening) + pure
  `ManagedKeyScope::managed_key_basename` (unit-tested). Provisioning-path
  wiring (provider_factory/vps/machine0 selecting the scoped key) is
  DEFERRED — the pure naming + default land; the call-site selection is
  follow-up.
- [~] 5.2 `thegn secret ssh rotate [--account]` reports scope + the plan
  (generate → authorize → verify → de-authorize → retire, both-keys-live on
  partial failure) and emits an audit breadcrumb. The live per-instance
  authorize across provider transports is DEFERRED.
- [ ] 5.3 Destroy paths record the authorized managed key — DEFERRED.

## 6. Host-key policy table

- [x] 6.1 `thegn-core/src/hostkey.rs`: `HostKeyClass` (4 classes) + policy
      table + argv chokepoint (`host_key_args`/`host_key_opts_str`,
      `forward_agent_allowed`), pure + unit-tested.
- [x] 6.2 Migrate call sites: `vps/ssh_shim.rs` (ManagedFresh), `host/mod.rs`
      (LoopbackTunneled+alias), `envplan.rs` bootstrap (SandboxBootstrap),
      `agent_ssh.rs`/`agent.rs` (LoopbackTunneled), `autoscale.rs`
      (ManagedFresh). `remote.rs ssh_base` = UserDeclared (adds nothing).
- [x] 6.3 Host-key ratchet ENFORCED as a Rust test in each crate's
      `platform_ratchet_tests.rs` (`host_key_literals_stay_in_the_chokepoint`,
      via `file_ratchet`), running in `just test` (the pre-push gate) and
      regenerated by `just ratchet-update`. Allowlists
      `test/hostkey-{core,svc,host}-ratchet.txt` (core pins the 2 extra_args
      test fixtures; svc/host empty). Forbidden set defined once next to the
      chokepoint (`hostkey::is_host_key_literal`).
- [x] 6.4 Doctor: class → policy → justification table (`hostkey_report`).

## 7. Exposure policy

- [x] 7.1 Sealed/SealedTunnel clamp: `SSH_AUTH_SOCK` dropped at the env fold
      (`SandboxProfile::seals_agent_socket`); `/run/user` removed from the
      default mounts entirely (tightened beyond spec per approval → applies to
      Hardened too). Tests added.
- [x] 7.2 No-agent-forwarding for ManagedFresh + LoopbackTunneled
      (`forward_agent_allowed`); `forward_agent` default flipped to false
      (approved tightening) for user-declared too.
- [x] 7.3 Doctor per-tier secret-exposure listing (`exposure_report`).

## 8. Signing

- [x] 8.1 `[identities.<name>.signing]` (`format`, `key`) on `IdentityConfig`;
      `identity::resolved` → `git_signing` + `git_signing_args`
      (`-c gpg.format=… -c user.signingKey=…`); unit-tested. (Splicing into the
      pane/compose fold: helper ready; call-site wiring is follow-up.)
- [~] 8.2 Interplay documented in the config example (commit-overlay + `[git]
override_gpg` layer above the identity default). Merge-queue fold-commit
  signing doc: brief.

## 9. Docs + config surface

- [x] 9.1 `config/config.toml.example`: `[credentials]`, `[credentials.ssh]`,
      `[identities.<name>.signing]` + the sandbox-tightening notes.
- [~] 9.2 `docs/help/` — no new actions/keybinds; signing prose is in the
  config example; dedicated help-page prose is minimal (follow-up).
- [x] 9.3 `docs/extending/provider-impl.md` — `SecretStore` backend recipe.

## 10. Gate

- [ ] 10.1 `just ci` — NOT run (left for the reviewer per box discipline);
      scoped tests + the host-key ratchet were run green. See the final report.
