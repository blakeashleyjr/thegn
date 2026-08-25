# Design — credential broker

The security section is not an appendix here; the whole design is a security
boundary. It is organized as: inventory (what exists), the typed ref, the
broker seam, SSH identity custody, the host-key policy table, exposure policy,
signing, audit, then the threat model that justifies each decision.

## 1. Inventory — every secret class thegn touches today

| Class                                                | Where it lives                         | Resolution today                                                                             | Gap                                                                                 |
| ---------------------------------------------------- | -------------------------------------- | -------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------- | ----------------- | ------------------------- |
| Provider API tokens (fly/DO/hetzner/daytona/sprites) | `[env.<n>.provider] api_key_env`       | `thegn-host/src/secret.rs::resolve` — `keyring:`/`env:`/`file:`/bare⇒env-name                | the only fully-layered path; host-only                                              |
| Issue-tracker tokens (Linear/Jira/Kaneo)             | `[[issues.accounts]] token`            | `expand_env_ref` — `env:`/`file:`/bare⇒**literal value**                                     | plaintext accepted silently; no `keyring:`                                          |
| CI provider tokens (GitLab)                          | `[ci.<kind>] token`                    | `expand_env_ref`                                                                             | same                                                                                |
| Forge auth                                           | `gh` CLI's own store (`GH_CONFIG_DIR`) | delegated to `gh`; profile firewall drops `GH_TOKEN`/`GITHUB_TOKEN` from the launching shell | pane passthrough re-adds `GH_TOKEN` by default                                      |
| Kaneo device-flow token                              | state DB (`get_kaneo_token`)           | direct DB read                                                                               | a secret **value** in the cache DB — violates the hard rule this change writes down |
| Managed SSH key                                      | `$XDG_STATE/thegn/ssh/sprite_ed25519`  | `sprite_ssh_keypair()`                                                                       | one key ⇒ every managed remote; no rotation                                         |
| User SSH identities                                  | `~/.ssh`, agenix/sops tmpfs symlinks   | `ssh_creds.rs` flatten + `identity_mounts`                                                   | good; stays                                                                         |
| GPG homes                                            | `GNUPGHOME` per profile/identity       | env pinning                                                                                  | `~/.gnupg:rw` in default sandbox mounts                                             |
| Iroh node key                                        | keyring via `secret::store`            | `iroh_home.rs`                                                                               | fine; migrates to broker naming                                                     |
| Snapshot store creds                                 | injected `&                            | r                                                                                            | secret::resolve(r)`                                                                 | closure injection | the pattern to generalize |
| VPN auth keys                                        | `[sandbox.vpn]` refs                   | `expand_env_ref`                                                                             | same divergence                                                                     |
| Control-plane pairing token                          | `[serve]`/pairing                      | control config                                                                               | consumed by `add-remote-enqueue-modes` injection                                    |

Two conclusions fall out. First, the _mechanisms_ are fine — the layered
store, the firewall, the stdin discipline are each correct where they apply.
Second, every gap is a _routing_ problem: fields that never reach the layered
store, policies that never reach a table. So the broker is a chokepoint and a
type, not a new storage engine.

## 2. The typed SecretRef

```rust
// thegn-core/src/secretref.rs — pure, no I/O, 95%-covered
pub enum SecretRef {
    Keyring { account: String },   // "keyring:<account>"
    Env     { var: String },       // "env:VAR"  (and bare-as-env fields)
    File    { path: String },      // "file:PATH" (~ expanded; agenix/sops land here)
    Literal { value_redacted: () },// bare string on a legacy literal-token field
}
```

- **Parsing is per-field-family but explicit.** `SecretRef::parse(s, BareAs)`
  takes a `BareAs::{EnvName, Literal}` marker so the two historic bare-string
  meanings are preserved _and named_. Serde keeps the config schema as
  `String` (no config-format change, no home-manager module churn); parse
  happens once at load into the typed form carried on the config structs'
  accessors.
- **`Literal` is deprecated, not broken.** It resolves exactly as today, but
  `thegn config validate` emits a warning naming the field and the fix, and
  `thegn secret migrate` stores the value (keyring, else `0600` file via the
  existing writer) and rewrites the field to the returned ref through
  `config_write` (comment-preserving `toml_edit`). `Literal` never `Display`s
  its value; `Debug` is manually implemented to redact — this is what makes
  "no secret in logs" unit-testable rather than aspirational.
- The `value_redacted` marker above is illustrative: the real type carries the
  value privately with redacted `Debug`/no `Display`/no `Serialize`, so it can
  still resolve.

## 3. The broker seam (shared with add-mcp-proxy-hub)

`add-mcp-proxy-hub` task 4.1 already scopes `secret::SecretStore` (object-safe
`get`/`set`/`del`/`list` under a thegn service namespace, Probe, kinds
implemented-or-reserved). This change **adopts that exact seam** and widens
its use from MCP upstream env to every secret field; there must never be two
secret seams. Coordination rule: whichever change lands first creates the
trait + keyring backend; the other consumes it (both tasks.md files say so).

- **Backends / kinds**: `keyring` (the existing `secret.rs` keyring leg, with
  its bounded probe and presence memo — those survive unchanged), `file`
  (`0600` file under the config-adjacent secrets dir, plus arbitrary
  `file:PATH` reads — agenix/sops need nothing special: their tmpfs files are
  `file:` targets and the sandbox side is already handled by
  `ssh_creds::identity_mounts`' symlink walking), `env`. `exec` (external
  command à la `pass`/`op`) is declared **reserved**: accepted by config,
  rejected by `--strict` validation, no sub-table until implemented — per the
  provider-seams recipe.
- **Errors are `SeamError`-classed**, not `anyhow` strings, so doctor and
  callers can distinguish `Unavailable` (no Secret Service) from `Denied`
  (locked keychain) from `NotFound` (unset var). This also fixes the pattern
  the remote map flagged on `RemoteProvider`.
- **Placement**: the trait + pure parse/policy logic in `thegn-core`
  (substrate-free — the trait's methods are sync and object-safe; keyring FFI
  and file I/O live in the host/svc impls). Core and svc consumers that need
  resolution at runtime take an injected `&dyn Fn(&SecretRef) -> …` or
  `&dyn SecretStore`, the pattern `thegn_svc::snapshot::open_store` already
  uses; `thegn-svc/src/issue/mod.rs` and the CI clients switch from
  `expand_env_ref` to the injected resolver.
- **Doctor**: a `Secrets` section — one Probe row per backend
  (keyring/file/env, exec reserved) plus a per-configured-ref line: field,
  backend, `resolves`/`missing` (presence only, cached — reusing
  `resolve_present_cached` so doctor doesn't hammer the Secret Service).
- **Catalog**: `secret.set` / `secret.rm` / `secret.list` / `secret.migrate` /
  `secret.audit` / `secret.ssh.rotate` rows, `SurfaceSet::OPERATOR` (CLI +
  control API; **not** MCP, not plugins — same rationale as the pinned
  admin-caps test: a tool-calling agent must not be able to enumerate or
  rewrite secret custody). `list` returns ref names and backends, never
  values; there is deliberately no `secret.get` surface at all — the broker
  resolves for _components_, not for callers.

## 4. SSH identity management

**Selection** already exists and is not rebuilt: `[env.<name>.ssh] identity`
(explicit `-i`), identities' `git.ssh_key` with `IdentitiesOnly=yes`, profile
`ssh/id` fallback. The broker adds custody and lifecycle for the _managed_ key
material:

- **Evaluating the single shared key.** Today's `sprite_ed25519`: the private
  key never leaves the host, so a compromised remote gains only its own
  authorized*keys line — the real risks are (a) any same-user local process
  (or any pane with `FileAccess::All`/`Host`, or the default state-dir
  reachability) reads one file and gains **every** managed remote, and (b)
  there is no way to revoke one provider account's access without touching
  all of them. A per-\_instance* key would make (a) no worse and (b) perfect,
  but costs a keygen + authorize round-trip on the hot provisioning path and
  breaks the warm-spare/checkpoint reuse model (a recycled sprite's
  authorized_keys must match). **Decision: per-provider-account keys**
  (`thegn/ssh/<provider>-<account>_ed25519`) as the new default for newly
  provisioned instances behind `[credentials.ssh] managed_key_scope =
"per-account"`, with `"shared"` remaining the config default until the
  rotation verb has soaked one release — flipping a custody default silently
  would strand existing fleets.
- **Rotation**: `thegn secret ssh rotate [--account <a>]` generates the
  replacement, appends its pubkey to every live instance of that scope (via
  the existing exec transports), verifies a connect with the new key, removes
  the old pubkey line, then retires the old private key. Partial failure
  leaves both keys authorized and says so (never brick a fleet mid-rotate).
- **Revocation notes**: instance/host destroy paths already delete
  per-instance known_hosts; they additionally note (audit event) which managed
  key had been authorized there, so `secret audit` can answer "where does this
  key still work".
- **ssh-agent** is treated as _key custody_, not a secret-store backend:
  managed keys may optionally be loaded into the user's agent instead of read
  from disk (future work, noted as an open question) — but the agent is never
  used to satisfy `keyring:` refs.

## 5. The host-key policy table

The three observed policies become four named connection classes with one
policy each, defined in `thegn_core::hostkey` (pure) and consumed by a single
argv-builder every call site goes through:

| Class                                                             | Policy                                                                                                                                     | Justification                                                                                                                                                                                                                                                                                                                    |
| ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `UserDeclared` (user's `[host.*]`, `[env.*.ssh]`, GitLoc remotes) | defer to the user's ssh config; thegn adds `accept-new` **only** when creating a config-less bootstrap context (in-sandbox git, fresh env) | the user's `~/.ssh` trust store is authoritative; thegn must not loosen it                                                                                                                                                                                                                                                       |
| `ManagedFresh` (VPS/machine0 just provisioned)                    | `accept-new` + per-instance known_hosts file, deleted with the instance                                                                    | first contact is unauthenticated by construction; pinning starts at first key and cannot pollute the global file                                                                                                                                                                                                                 |
| `LoopbackTunneled` (sprite SSH-over-WSS, iroh dumbpipe)           | host-key check disabled (`no` + `/dev/null`) or `HostKeyAlias` pin where stable                                                            | endpoint identity is established by the outer authenticated transport (WSS auth / iroh ticket); an inner TOFU pin against `127.0.0.1:<port>` would produce false mismatches across port churn — iroh's `HostKeyAlias` variant is the better form and `LoopbackTunneled` sites SHOULD adopt it where the inner host key is stable |
| `SandboxBootstrap` (envplan's in-sandbox `core.sshCommand`)       | `accept-new`, global-scope inside the sandbox only                                                                                         | a fresh sandbox has no known_hosts; the write stays inside the sandbox home                                                                                                                                                                                                                                                      |

Enforcement is house-pattern: the builder is the chokepoint (like
`wire.rs::color_spec` for colors), and a new shrink-only ratchet
(`test/hostkey-ratchet.txt`, checked in `just lint`) lists the legacy sites
until they migrate; no new `StrictHostKeyChecking`/`UserKnownHostsFile`
literal may appear outside the chokepoint. `thegn doctor` prints the table
with the class each live config resolves to.

## 6. Exposure policy — panes, tiers, agent forwarding

Today's defaults hand more than intended to sandboxed code (see threat model):

- **Sealed / SealedTunnel** profiles get a default-deny secret posture:
  `SSH_AUTH_SOCK` is dropped from `env_passthrough`, and `/run/user` leaves
  the default mounts for those tiers (it carries the session bus ⇒ Secret
  Service ⇒ the OS keyring — mounting it into a "sealed" sandbox contradicts
  the tier's promise). Users can re-add either explicitly; doctor flags it
  when they do. **Hardened and Open keep today's behavior** (a default flip
  there breaks everyday git-over-ssh in panes); instead `thegn doctor` gains a
  per-tier "secret exposure" listing so the trade is visible.
- **Agent forwarding**: `forward_agent` keeps its `true` default for
  `UserDeclared` hosts (back-compat; the user chose those hosts), and the
  provisioning/exec paths for `ManagedFresh` + `LoopbackTunneled` instances
  force `-a` (no forwarding) — a managed ephemeral box has no business
  signing with the user's agent. The interactive `connect = "ssh"` sprite pane
  documents that enabling forwarding there extends agent reach into the
  sandbox.
- The `.env` credential-shaped-key filter and clear-then-allowlist pane env
  (roadmap 744, H 105) are unchanged; the broker adds no new env injection.

## 7. Signing

- `[identities.<name>.signing]`: `format = "openpgp" | "ssh"`, `key =
"<keyid|path>"` — additive to `add-decoupled-identities`' `IdentityConfig`
  (already in-tree, `thegn-core/src/identity.rs`); resolution emits
  `git -c gpg.format=… -c user.signingKey=…` alongside the existing
  `GNUPGHOME`/`GIT_SSH_COMMAND` bindings, at the same fold point
  (`bundle::compose` / pane spawn — off-loop).
- **Worktree-level override is free**: the identity switcher already binds
  per-worktree; a worktree bound to identity `release` signs with `release`'s
  key while its siblings don't. No new UI.
- The commit overlay's `^S` inherit→sign→no-sign cycle and `[git]
override_gpg` (disable signing during rewrites to avoid passphrase stalls)
  are preserved as the _operation-level_ layer above the identity default.
- **Merge-queue folds**: fold commits are created by plumbing in the gate
  worktree and inherit that worktree's resolved git config — with a signing
  identity bound at repo scope, folds sign. Left as documented behavior, not
  a queue feature (a queue-level signing knob is an open question).

## 8. Audit trail

- Every broker resolution emits one structured event, target
  `thegn::secret::audit`: `{ref_name, backend, consumer, outcome}` where
  `consumer` is a static component tag (`provider:fly`, `issues:linear`,
  `snapshot`, `mcp:<upstream>`…) passed by the caller. **Values never appear**;
  a unit test asserts the event type's `Display`/serde output for a
  known-secret fixture contains no secret bytes (enforced, not promised).
- Free when off: no subscriber ⇒ no cost (house instrumentation rule);
  resolution already happens off the render loop (spawn/provision/hydration
  paths), and the presence-memo keeps keyring traffic bounded — no new wake
  source, no event-loop involvement. Render damage: none (doctor/CLI only).
- `thegn secret audit` prints the configured refs with backend + last outcome
  (from the doctor presence pass, not a persistent log). A persistent JSONL
  audit sink under the state dir is **optional and off by default**
  (`[credentials] audit_file = false`) — it stores metadata only, and the DB
  is deliberately not used (the DB is a cache; and the hard rule keeps secret
  _material_ out of it — which also means migrating the Kaneo device-flow
  token out of the DB into the store, flagged in tasks).

## 9. Threat model — what a compromised pane/agent gets, per tier

"Compromised pane" = arbitrary code running in a worktree pane (malicious
build script, prompt-injected agent in the `[[agents]]` picker, supply-chain
test). "After" assumes this change's defaults.

| Tier                                           | Today                                                                                                                                                                                                                                                                                                                  | After                                                                                                                                                                                                                                                              |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Open** (no sandbox)                          | Everything the user has: dotfiles, `~/.ssh` keys, state-dir managed key, `0600` secrets files (same uid), session bus ⇒ whole OS keyring, live agent socket.                                                                                                                                                           | Unchanged — same-uid access is not a boundary the broker can create; documented honestly. The keyring backend protects at rest and against _other users_, not against same-user code.                                                                              |
| **Hardened** (default)                         | Worktree+caches mounts, NAT net, **plus** by default: `GH_TOKEN`/`GITHUB_TOKEN`/`ANTHROPIC_API_KEY` env, `SSH_AUTH_SOCK` + `/run/user` (⇒ agent _and_ Secret Service reachable), `~/.gnupg:rw`. Can push as the user, sign with agent keys, read every `keyring:` secret via the bus, and tamper with the GPG keyring. | Same capability set (back-compat) but **enumerated by doctor** per tier, so it is a visible decision; profile/identity firewall still narrows which accounts those tools resolve to.                                                                               |
| **Sealed / SealedTunnel**                      | As Hardened for env/mounts defaults (tiers differ on network).                                                                                                                                                                                                                                                         | Agent socket and `/run/user` dropped by default ⇒ no OS-keyring reach, no agent signing; forge tokens still pass only if the user's `env_passthrough` says so. A sealed pane's blast radius: the worktree, its caches, and whatever tokens were explicitly passed. |
| **Provider / remote placements**               | `passthrough_env_remote` drops socket vars (good) and streams exports via stdin (good); remote holds `GH_TOKEN`/`ANTHROPIC_API_KEY` in process env for its lifetime; remote holds only the managed _public_ key.                                                                                                       | Same, plus: per-account managed keys mean a compromised provider account's instances can be cut off by one rotation; audit records which components pulled which tokens into which env.                                                                            |
| **Agent handoff (merge/pr queue) & fold gate** | Subprocess of thegn: inherits thegn's whole env (profile-firewalled but includes resolved provider vars present in the process). Runs under `wrap_background_argv` resource caps, not a sandbox.                                                                                                                       | Unchanged mechanically; audit events name `agent_task` as consumer for anything it resolves; hard-rule requirement forbids passing secrets in the rendered prompt/argv template.                                                                                   |
| **A compromised _remote_ (managed instance)**  | Gets: its own workspace copy, streamed env exports, the outer transport creds for itself. Cannot read the local keyring or other instances' keys. With `forward_agent=true` on user hosts: can sign/auth as the user while connected.                                                                                  | Managed classes force no-forwarding; user-declared hosts keep the user's choice with the risk documented.                                                                                                                                                          |

Residual risks accepted and stated: same-uid access on Open/no-sandbox;
Secret-Service's lack of per-app ACLs (any same-session client reads all items
once unlocked) — the mitigation is the tier mount/env policy, not keyring
namespacing; `Literal` refs continue to resolve until migrated (warned, not
broken).

## 10. Alternatives considered

- **Delegate everything to an external manager (`pass`, `op`, vault agent).**
  Rejected as the _only_ path: thegn must degrade to env/file on headless CI
  boxes and must not hard-depend on a vendor CLI (seams-not-vendors). It
  arrives instead as the `exec` reserved backend kind.
- **A broker daemon holding secrets in memory, handing FDs to consumers.**
  Strongest isolation (panes never see values), but a large IPC surface and a
  new privileged process; the pane daemon seam could host it later. The typed
  ref + chokepoint is the prerequisite either way — deferred, noted in open
  questions.
- **Make `SecretRef` a serde newtype in config structs now.** Cleaner types,
  but churns the JSON schema, the home-manager module, and every config test
  for zero user-visible gain over parse-at-load accessors. Revisit when a
  config-format major rev happens (`align-config-formats-and-validation`).
- **Unify bare-string semantics to one meaning.** Would silently break either
  every existing `api_key_env = "FLY_API_TOKEN"` (bare⇒env) or every pasted
  literal token (bare⇒literal). Per-field `BareAs` markers + deprecation
  warnings are uglier but honest.
- **Per-instance managed SSH keys.** Better revocation granularity than
  per-account, rejected for now: hot-path provisioning cost and
  checkpoint/warm-spare authorized_keys mismatch (see §4).

## 11. Open questions

1. Should the `exec` backend land in this change or stay reserved? (Reserved
   recommended — the seam is the deliverable.)
2. Queue-level signing knob for merge-queue fold commits (team-mode want), or
   is repo-scope identity binding enough?
3. Agent-held managed keys (`ssh-add` into the user agent, key files never on
   disk unencrypted) — worth it once rotation exists?
4. Does `add-config-trust-resolution` need a rule that repo-layer (`.thegn.*`)
   config may _reference_ secrets but never widen exposure (e.g. adding
   `env_passthrough` entries is an additive-request, TOFU-gated)? Flagged to
   that change's owner; this design assumes yes.
5. The in-memory broker daemon (panes get sockets/FDs, never values) — future
   change on top of the pane-daemon seam?
