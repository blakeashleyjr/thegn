# Tasks — add-credential-broker

Iterate with `just quick <crate>` and targeted `cargo nextest run -p <crate>
<substring>`; the full gates run once at the end (dev-loop policy).

## 1. Typed SecretRef (pure core)

- [x] 1.1 `thegn-core/src/secretref.rs`: `SecretRef` enum
      (Keyring/Env/File/Literal), `parse(s, BareAs)` with
      `BareAs::{EnvName, Literal}`; redacted `Debug`, no `Display`/serialize
      of literal values. Unit tests incl. the redaction sentinel test
      (95% core gate).
- [x] 1.2 Parse-at-load: `thegn-core/src/secret_scan.rs` enumerates every
      configured secret field into typed `SecretRef`s with the right per-field
      `BareAs` (provider `api_key_env` = EnvName; issue/CI tokens = Literal).
      Config schema stays `String`. VPN keys, snapshot S3 credentials, and MCP
      upstream env are included with stable consumer tags.
- [x] 1.3 `thegn config validate`: warning for the legacy issue-account and
      GitLab CI `Literal` fields naming the field + fix (`cmd/config.rs`,
      advisory/non-failing); `secret_scan` tests pin that exact migration
      boundary. VPN, snapshot, and MCP refs are broker-resolved but are not
      silently advertised as auto-rewritable.

## 2. Broker seam + backends

- [x] 2.1 `SecretStore` seam (`thegn-core/src/secret_store.rs`): object-safe
      get/set/del/list, seam-classed `SecretError`
      (unavailable/denied/not-found), `kind` implemented-or-reserved
      (`keyring`, `file`, `env`; `exec` reserved). (First-lander built it.)
- [x] 2.2 Rehome `thegn-host/src/secret.rs`: `KeyringStore`/`FileStore`/
      `EnvStore` impls (hard deadlines on keyring get/set/delete/probe, presence
      memo stores only booleans, and file writes publish atomically with strict
      owner-only permissions); broker chokepoint `resolve_ref_for` used by
      `resolve`/`resolve_for`. Hashed fallback filenames prevent sanitizer aliases.
- [x] 2.3 Svc consumers use the host-installed typed resolver
      (`thegn-svc/src/secret.rs`): issue, CI, and VPN no longer independently
      fetch refs; snapshot keeps an explicit typed closure; MCP upstream env
      funnels through `resolve_ref_for`. GitLab receives its resolved token via
      child environment, and VPN launches use a 0600 env-file rather than
      exposing values in the OCI argv.
- [x] 2.4 Kaneo device-flow token moved OUT of the state DB into the broker:
      `thegn kaneo login` stores the raw token via the broker (0600 file) and
      records only a `file:` SecretRef in `kaneo_auth` (`persist_kaneo_token`);
      the svc read path resolves it via `expand_env_ref` with a read-through
      fallback for legacy raw-token rows. Tests assert no raw token lands in the
      DB row (and a store that echoes the raw value is rejected).
- [x] 2.5 Doctor: Secrets section — backend probe rows (`secret::probes()`) +
      per-ref presence lines (via `secret::present`); `exec` shown reserved.
- [x] 2.6 `thegn secret migrate`: legacy issue-account/GitLab CI literal →
      0600 file + `config_write` rewrite (`set_issue_account_token` / `set_key`),
      `--dry-run`. Other brokered field families require an explicit ref edit.

## 3. CLI + catalog

- [x] 3.1 `thegn secret set|rm|list|migrate|audit` + `thegn secret ssh rotate`
      (`cmd/secret.rs`); `set` reads stdin (never argv); `list`/`audit` are
      names+backends+presence only — no value-read verb. `ssh rotate` executes
      the recorded live-scope transaction; `--dry-run` is value-free.
- [x] 3.2 CATALOG rows for each verb (`secret.set/rm/list/migrate/audit/
ssh.rotate`), `SurfaceSet::OPERATOR`, Admin `required_scope`; Http/Grpc
      `SURFACE_GAPS`, CLI coverage in `cli_control_caps`; admin-caps test green.
- [x] 3.3 MCP secret delegation is intentionally excluded from this contract;
      no live broker catalog delegates secret administration to MCP callers.

## 4. Audit trail

- [x] 4.1 Audit event type (`thegn-core/src/secret_audit.rs`) +
      `thegn::secret::audit` tracing at the chokepoint (ref name, backend,
      consumer tag, outcome). Provider, issue, CI, VPN, snapshot, MCP, and
      managed-SSH lifecycle callers pass component-specific tags.
- [x] 4.2 Redaction sentinel tests (secretref + secret_audit: sentinel absent
      from Debug / serde / audit_name).
- [x] 4.3 Removed the dormant `[credentials] audit_file` config/schema/example
      promise. The bounded audit contract is tracing metadata plus the live
      presence pass used by `secret audit`; there is no pretend persistent sink.

## 5. SSH identity custody

- [x] 5.1 `[credentials.ssh] managed_key_scope` (config + `config_enum`,
      default **per-account** per the approved tightening) + pure
      `ManagedKeyScope::managed_key_basename` (unit-tested). Provider factories
      select the scoped key from the provider's value-free credential-ref identity;
      startup and live reload both publish the top-level scope.
- [x] 5.2 `thegn secret ssh rotate [--account]` implements generate → authorize
      everywhere → replacement-key connection proof → promote → de-authorize old →
      retire for VPS/Fly/machine0/Sprites. Sprites custody persists the value-free
      originating worktree and proves the staged key with a real OpenSSH handshake
      over `sprite-proxy`, bound to the exact provider/account/instance tuple;
      pre-field records fail closed before mutation. Every proof disables
      ControlMaster reuse. Value-free recovery custody is persisted before the
      first remote mutation and survives interruption/promotion boundaries.
      De-authorization rollback restores old access everywhere before reverting
      local custody; if that rollback is incomplete, the working replacement
      stays canonical and the retiring key remains tracked for later cleanup.
- [x] 5.3 The secret-free `managed_ssh` custody ledger records exact key path +
      fingerprint plus optional worktree-bound proxy/recovery context after
      verified authorization and is removed only after successful/idempotent
      provider destroy. Record identity and all host/svc APIs are scoped by the
      exact provider/account/instance tuple; ambiguous legacy bridge lookups fail
      closed. Missing custody is distinct from unreadable/malformed custody;
      complete inventory and mutation paths propagate corruption and preserve
      its evidence. VPS, Fly, machine0, and Sprites destroy paths emit value-free
      revocation events. Per-account basenames include a stable identity hash,
      preventing sanitized aliases; shared keeps `sprite_ed25519`.

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

## 8. Signing scope boundary

- [x] 8.1 Preserve the already-landed `[identities.<name>.signing]` schema and
      pure resolution helper as groundwork, without claiming that a pane or Git
      subprocess consumes it.
- [x] 8.2 Move runtime injection, signing-enable semantics, worktree-isolation
      evidence, operation-level precedence, and dedicated help prose to the
      focused `bind-identity-commit-signing` change. The complete requirement
      and scenario live there; no signing execution requirement remains hidden
      in this credential-custody delivery.

## 9. Docs + config surface

- [x] 9.1 `config/config.toml.example`: `[credentials.ssh]`,
      `[identities.<name>.signing]` + the sandbox-tightening notes.
- [x] 9.2 Broker migration and operator behavior are documented in
      `MIGRATION.md`, the config example, generated CLI reference, and provider
      extension guide. Identity-signing help is owned by the explicitly split
      `bind-identity-commit-signing` change.
- [x] 9.3 `docs/extending/provider-impl.md` — `SecretStore` backend recipe.

## 10. Gate

- [x] 10.1 `env RUSTC_WRAPPER= just ci` — passed on 2026-09-10.
