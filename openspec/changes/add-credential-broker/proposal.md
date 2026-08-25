# Add a credential broker: typed secret refs, SSH identity + host-key policy, signing, audit

Linear: THE-66

## Why

thegn already touches every class of developer credential — provider API
tokens, forge auth, issue-tracker keys, SSH identities, GPG homes — but there
is no one place that owns them. The issue title asks the right question
("Credential Broker? SSH Identity Manager? GPG?"); the codebase answers with
five disconnected mechanisms and several verified gaps:

- **Two resolution layers with opposite bare-string semantics.**
  `thegn-host/src/secret.rs::resolve` handles `keyring:` / `env:` / `file:`
  and treats a bare string as an **env-var name**. `thegn_core::config::
expand_env_ref` handles `env:` / `file:` and returns any other non-empty
  string **as the literal secret value**. Issue-tracker tokens
  (`IssueAccount.token`, `thegn-svc/src/issue/mod.rs:157–176`) and CI tokens
  go through the latter — so a raw Linear/Jira API key pasted into
  `config.toml` is silently accepted as plaintext, and `keyring:` refs do not
  work for them at all. Which fields accept which schemes is undiscoverable.
- **One managed SSH key for every managed remote.** `sprite_ssh_keypair()`
  (`thegn-host/src/agent_ssh.rs:28`) generates a single unencrypted ed25519
  key under `$XDG_STATE/thegn/ssh/` whose public key is authorized on every
  sprite, Fly, DigitalOcean, Hetzner and machine0 instance ever provisioned.
  There is no rotation verb, no per-account scoping, and no revocation story
  beyond destroying instances.
- **Three host-key policies coexist with no policy table** (remote-map finding
  8): accept-new + per-instance `known_hosts` (`thegn-svc/src/vps/ssh_shim.rs`),
  accept-new against the global file (`thegn-svc/src/host/mod.rs:181`,
  `envplan.rs:1477`), and `StrictHostKeyChecking=no` + `/dev/null`
  (`agent_ssh.rs:184`, `agent.rs:2129` — justified by the authenticated
  WSS/proxy transport underneath, but the justification lives in scattered
  comments, not a checkable policy).
- **Secret exposure to panes is a silent default.** The default
  `[sandbox] env_passthrough` hands `GH_TOKEN`, `GITHUB_TOKEN`,
  `ANTHROPIC_API_KEY` and `SSH_AUTH_SOCK` to every sandboxed pane; the default
  mounts include `~/.gnupg:rw` and `/run/user` (which contains the session bus
  — i.e. the OS **keyring is reachable from inside a Hardened sandbox**);
  `[env.<name>.ssh] forward_agent` defaults to `true`. None of this is
  surfaced anywhere.
- **No audit trail.** Nothing records which component resolved which secret
  when, so a leak investigation has nothing to replay and `thegn doctor`
  cannot say where a given token actually comes from.

What already exists is good and stays: the layered `secret.rs` store with its
keyring→file writer (roadmap 757, `add-env-setup-ux`), the profile credential
firewall (H 105), the decoupled `[[identities]]` primitive
(`add-decoupled-identities`, partially implemented in
`thegn-core/src/identity.rs`), the stdin-streaming discipline that keeps
secrets off argv (`thegn-svc/src/vps/ssh_shim.rs`), and per-instance
`known_hosts` for fresh VPSes. This change promotes those local disciplines
into one brokered, typed, audited layer — the missing owner the issue asks for.

## What Changes

- **New capability `credentials`** with a typed `SecretRef` in `thegn-core`
  (`Keyring` / `Env` / `File` / `Literal`), parsed once at config load.
  Bare-string semantics stay per-field for back-compat, but become explicit in
  the type; a `Literal` (raw secret in config plaintext) warns in
  `thegn config validate` and `thegn secret migrate` moves it into the keyring
  (or a `0600` file) and rewrites the config to hold only the ref, via the
  existing comment-preserving `config_write` path.
- **One broker chokepoint, backends as a provider seam.** A `SecretStore`
  seam (object-safe trait, caps ⇔ optional ops, `kind`
  implemented-or-`reserved`, `Probe` in `thegn doctor`) with `keyring`, `file`
  (covers agenix/sops tmpfs files via the existing symlink discipline) and
  `env` backends; `exec` (pass/1Password-style external command) is declared
  `reserved`. This is the **same seam `add-mcp-proxy-hub` task 4.1 scopes** —
  one seam serves both; its `thegn mcp secret` verbs become a namespaced view
  over the same store. All consumers migrate to the chokepoint: provider
  tokens (already there), issue accounts, CI tokens, snapshot store, iroh
  identity, VPN auth keys, MCP upstream env. Core/svc stay substrate-free by
  taking an injected resolver exactly as `thegn_svc::snapshot::open_store`
  already does.
- **SSH identity management.** Per-remote/per-workspace key selection already
  exists (`[env.<name>.ssh] identity`, identities' `git.ssh_key`); this adds
  managed-key scoping (`[credentials.ssh] managed_key_scope = "shared" |
"per-account"` — new instances get per-provider-account keys; the existing
  shared key keeps working for existing instances), `thegn secret ssh rotate`
  (generate + re-authorize + retire), and revocation notes on instance/host
  destroy paths.
- **One host-key policy table.** A `HostKeyPolicy` enum with four connection
  classes (user-declared host / managed fresh instance / loopback-over-
  authenticated-transport / in-sandbox git bootstrap), a single argv-builder
  chokepoint every ssh call site names its class through, a shrink-only
  ratchet (`test/hostkey-ratchet.txt`) that no `StrictHostKeyChecking` literal
  appears outside it, and a `thegn doctor` table printing class → policy →
  justification.
- **Agent-forwarding and pane secret exposure become policy.** Sealed and
  SealedTunnel sandbox profiles stop passing `SSH_AUTH_SOCK` (and the
  `/run/user` session-bus mount) by default; Hardened keeps today's behavior
  but `thegn doctor` lists exactly which secrets each tier would expose.
  `forward_agent` keeps its default for user-declared hosts and is forced off
  for managed ephemeral instances.
- **Signing per identity.** `[identities.<name>.signing]` gains
  `format = "openpgp" | "ssh"` and `key`; resolution follows the existing
  identity scope chain (worktree binding → workspace → global → repo git
  config), so a worktree-level override is the already-shipped identity
  switcher. The commit overlay's `^S` inherit→sign→no-sign cycle (roadmap 328)
  is unchanged; SSH-format signing reuses the same key custody as the SSH
  identity manager.
- **Audit trail without values.** Every broker resolve emits a structured
  tracing event (`thegn::secret::audit`: ref name, backend, consumer
  component, outcome — never the value); `thegn secret audit` summarizes
  configured refs, their backends and last-resolution outcomes. Free when no
  subscriber is installed (house instrumentation rule).
- **Hard rules as requirements**: no secret value in config plaintext, argv,
  logs, or the SQLite DB; subprocesses receive secrets via environment or
  stdin only (the `ssh_shim` stdin-export discipline generalized to a
  requirement).
- **Catalog**: every new verb (`secret set|rm|list|migrate|audit`,
  `secret ssh rotate`) is a `thegn_core::capability::CATALOG` row on the
  OPERATOR surface set (CLI + control API; off MCP and plugins, matching the
  pinned admin-caps test), gated by `required_scope(verb)` — no second policy
  table.

## Impact

- **Roadmap**: group **H** items 104/105 (shipped — extended, not reopened);
  **AJ 431** (credential brokerage — agents never see raw keys: this delivers
  the shell-side, proxy-free subset), **440** (per-profile credential
  isolation), **328** (commit signing — per-identity key selection added),
  **628/675** (jj / signed tags — the signing identity they will consume),
  **757** (layered secret store — generalized). Group J remote work consumes
  the host-key table.
- **Specs**: new `credentials` capability; **MODIFIED** `sandbox` "Provider
  secrets resolve through a layered store" (resolution becomes the typed
  broker + audit; scenarios preserved).
- **In-flight changes reconciled**: `add-mcp-proxy-hub` (shares the one
  `SecretStore` seam — whichever lands first builds it, the other consumes
  it; its keyring entries live under the same namespaced service),
  `add-decoupled-identities` (the identity primitive this consumes — signing
  is an additive sub-table on its `IdentityConfig`), `add-env-setup-ux`
  (shipped `secret.rs` — kept as the host-side backend), `add-vps-providers` /
  `add-do-fly-providers` / `add-machine0-provider` (ssh_shim + managed keys +
  per-instance known_hosts), `add-remote-enqueue-modes` (the
  `THEGN_CONTROL_TOKEN` provision-time injection lands on broker custody),
  `add-sandbox-policy-engine` (tier exposure clamps are policy inputs),
  `add-config-trust-resolution` (repo-layer configs must not introduce secret
  refs that resolve in a trusted scope — see design), `add-cli-namespaces-and-
remote-open` (verb naming conventions for the `secret` namespace).
- **Cross-unit**: G7a (source-control identities) — forge/git account
  selection rides the same identity primitive; this change owns custody and
  policy, not account modeling. The in-progress MCP write-tools branch
  (`--scopes` gating) is upstream of the catalog rows here.
- **Code (indicative)**: `thegn-core/src/secretref.rs` (typed ref + parse +
  unify semantics, pure), `hostkey.rs` (policy table, pure),
  `thegn-svc/src/secret/` (seam + backends), `thegn-host/src/secret.rs`
  (becomes the keyring/file backend impl), `cmd/secret.rs`, doctor sections.
  No SQLite schema change, no new render surface, no new TUI actions.

## Phase 2 (tracked follow-ups, not in this change)

The security-critical core landed here (typed `SecretRef` + back-compat, the
one broker chokepoint + audit, the canonical redact seam, the host-key policy
table + enforced ratchet, the pane/agent-forwarding tightening, the Kaneo
token out of the DB, signing on the identity). These items are deliberately
deferred and tracked so they are not lost:

1. **Svc keyring-at-fetch (was task 2.3).** Issue-tracker and CI tokens still
   resolve via `expand_env_ref` (env:/file:, not keyring:) at fetch time; the
   typed vocabulary + `secret migrate` (to a `0600 file:`) already land. Inject
   the host's keyring-capable resolver into `thegn-svc/src/issue/` and the CI
   clients so a `keyring:` ref on those fields resolves. Until then, `secret
migrate` deliberately writes `file:` (resolvable today), not `keyring:`, for
   those fields — no silent breakage.
2. **Live SSH-key rotation + destroy-path key record (was 5.2/5.3).** The pure
   scoping + key-naming (`ManagedKeyScope::managed_key_basename`) and the
   `secret ssh rotate` plan/report land; the live per-instance authorize across
   provider exec transports (generate → authorize → verify → de-authorize →
   retire, both-keys-live on partial failure) and the destroy-path audit of the
   authorized managed key are follow-ups.
3. **Provisioning path key selection (was 5.1).** `provider_factory`/vps/machine0
   select the scoped managed key for newly provisioned instances (the helper +
   default are in place).
4. **Smaller:** the optional JSONL audit sink writer (`[credentials]
audit_file`, config flag present); VPN/snapshot/MCP fields added to
   `secret_scan`; splicing `identity::git_signing_args` into the pane/compose
   spawn fold; the `exec` secret backend (reserved).
