# Design — credential broker

The security section is not an appendix here; the whole design is a security
boundary. It is organized as: inventory (what exists), the typed ref, the
broker seam, SSH identity custody, the host-key policy table, exposure policy,
the signing scope boundary, audit, then the threat model that justifies each
decision.

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
- **`Literal` is deprecated, not broken.** It resolves exactly as today.
  `thegn config validate` warns for, and `thegn secret migrate` safely rewrites,
  the structurally bounded legacy issue-account and GitLab CI token fields.
  VPN, snapshot, and MCP maps use the same typed resolution seam but require a
  manual explicit-ref edit; their arbitrary user-defined map keys are not
  claimed as auto-rewritable. `Literal` never `Display`s its value; `Debug` is
  manually implemented to redact — this is what makes "no secret in logs"
  unit-testable rather than aspirational.
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
- **Placement**: pure parse/policy stays in `thegn-core`.
  `thegn-svc::secret` owns one typed process-injection seam installed by the
  host's `resolve_ref_for`; issue, CI, and VPN use it, snapshot accepts the same
  typed resolver explicitly, and MCP funnels directly through the host
  chokepoint. Svc-only tests retain env/file/literal compatibility resolution;
  `keyring:` fails closed without a host installation.
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
  (`thegn/ssh/<provider>-<account>_ed25519`) as the default for newly
  provisioned instances behind `[credentials.ssh] managed_key_scope =
"per-account"`; `"shared"` preserves the historic path when explicitly chosen.
  Existing instances keep their ledger-recorded key path until rotated.
- **Rotation**: `thegn secret ssh rotate [--account <a>]` generates the
  replacement, appends its pubkey to every live instance of that scope (via
  the existing exec transports), verifies a connect with the new key, removes
  the old pubkey line, then retires the old private key. VPS, Fly, and machine0
  prove a real replacement-key connection. Partial failure restores old access
  everywhere before local rollback; if that repair is incomplete, the verified
  replacement remains canonical. A value-free recovery entry is published
  before remote authorization and survives process interruption, tracking the
  auxiliary path/fingerprint and planned retiring path until cleanup completes.
  Sprites records the originating worktree as value-free custody context and
  proves the staged key through a hermetic local OpenSSH connection over its WSS
  ProxyCommand, which rejects any provider/account/instance mismatch after
  resolving the worktree. Older records missing that backward-compatible
  optional field refuse before mutation.
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

- **All sandbox tiers** omit `/run/user` from default mounts, removing the
  session-bus route to Secret Service. **Sealed / SealedTunnel** additionally
  drop `SSH_AUTH_SOCK` from `env_passthrough`; Hardened retains the variable for
  compatibility, but no socket is reachable without an explicit runtime-dir
  mount. Users can re-add either capability explicitly and doctor flags it.
- **Agent forwarding**: `forward_agent` defaults `false` for `UserDeclared`
  hosts and can be explicitly enabled there. `ManagedFresh` and
  `LoopbackTunneled` force `-a` regardless — a managed ephemeral box has no
  business signing with the user's agent.
- The `.env` credential-shaped-key filter and clear-then-allowlist pane env
  (roadmap 744, H 105) are unchanged; the broker adds no new env injection.

## 7. Signing scope boundary

`[identities.<name>.signing]` and the pure resolved-argument helper landed as
additive identity groundwork. This change does not inject those values into a
pane or Git subprocess and therefore does not claim identity-bound signing.
The complete execution contract — including whether a configured identity
enables signing, host and sandbox injection, worktree isolation, and precedence
under the commit overlay / `[git] override_gpg` — lives in
`bind-identity-commit-signing`.

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
- `thegn secret audit` prints configured refs with backend + current presence
  outcome. The dormant JSONL config field was removed: tracing subscribers are
  the persistence integration point, while the DB remains a cache and never
  stores secret material.

## 9. Threat model — what a compromised pane/agent gets, per tier

"Compromised pane" = arbitrary code running in a worktree pane (malicious
build script, prompt-injected agent in the `[[agents]]` picker, supply-chain
test). "After" assumes this change's defaults.

| Tier                                           | Today                                                                                                                                                                                                                                                                                                                  | After                                                                                                                                                                                                                                                    |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Open** (no sandbox)                          | Everything the user has: dotfiles, `~/.ssh` keys, state-dir managed key, `0600` secrets files (same uid), session bus ⇒ whole OS keyring, live agent socket.                                                                                                                                                           | Unchanged — same-uid access is not a boundary the broker can create; documented honestly. The keyring backend protects at rest and against _other users_, not against same-user code.                                                                    |
| **Hardened** (default)                         | Worktree+caches mounts, NAT net, **plus** by default: `GH_TOKEN`/`GITHUB_TOKEN`/`ANTHROPIC_API_KEY` env, `SSH_AUTH_SOCK` + `/run/user` (⇒ agent _and_ Secret Service reachable), `~/.gnupg:rw`. Can push as the user, sign with agent keys, read every `keyring:` secret via the bus, and tamper with the GPG keyring. | `/run/user` is absent, so the retained agent env variable cannot reach its socket and Secret Service is not exposed by default. GPG/env exposure remains and is enumerated by doctor. Agent forwarding defaults off unless a user-declared host opts in. |
| **Sealed / SealedTunnel**                      | As Hardened for env/mounts defaults (tiers differ on network).                                                                                                                                                                                                                                                         | `SSH_AUTH_SOCK` and `/run/user` are both absent by default ⇒ no OS-keyring reach and no agent signing; forge tokens pass only if explicitly allowed.                                                                                                     |
| **Provider / remote placements**               | `passthrough_env_remote` drops socket vars (good) and streams exports via stdin (good); remote holds `GH_TOKEN`/`ANTHROPIC_API_KEY` in process env for its lifetime; remote holds only the managed _public_ key.                                                                                                       | Same, plus: per-account managed keys mean a compromised provider account's instances can be cut off by one rotation; audit records which components pulled which tokens into which env.                                                                  |
| **Agent handoff (merge/pr queue) & fold gate** | Subprocess of thegn: inherits thegn's whole env (profile-firewalled but includes resolved provider vars present in the process). Runs under `wrap_background_argv` resource caps, not a sandbox.                                                                                                                       | Unchanged mechanically; audit events name `agent_task` as consumer for anything it resolves; hard-rule requirement forbids passing secrets in the rendered prompt/argv template.                                                                         |
| **A compromised _remote_ (managed instance)**  | Gets: its own workspace copy, streamed env exports, the outer transport creds for itself. Cannot read the local keyring or other instances' keys. With `forward_agent=true` on user hosts: can sign/auth as the user while connected.                                                                                  | Managed classes force no-forwarding; user-declared hosts default off and retain an explicit opt-in with the risk documented.                                                                                                                             |

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
2. Agent-held managed keys (`ssh-add` into the user agent, key files never on
   disk unencrypted) — worth it once rotation exists?
3. Does `add-config-trust-resolution` need a rule that repo-layer (`.thegn.*`)
   config may _reference_ secrets but never widen exposure (e.g. adding
   `env_passthrough` entries is an additive-request, TOFU-gated)? Flagged to
   that change's owner; this design assumes yes.
4. The in-memory broker daemon (panes get sockets/FDs, never values) — future
   change on top of the pane-daemon seam?
