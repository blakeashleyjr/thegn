# Migration — credential broker (THE-66)

This change tightens three security defaults. Everything keeps **resolving**
(no config silently breaks), but three behaviours change for existing
remote/agent workflows. `thegn doctor` now prints exactly what each sandbox
tier exposes, the host-key policy table, and a per-ref secrets section — start
there.

## 1. The OS keyring / session bus is no longer reachable from a pane

**What changed:** `/run/user` is no longer a default `[sandbox] mounts` entry.
It carried the user session bus (⇒ Secret Service ⇒ the OS keyring) and the
ssh-agent socket into **every** sandboxed pane (Hardened included), which
contradicted the sandbox's promise.

**Who is affected:** a pane (agent or shell) that reached the OS keyring, the
D-Bus session bus, the ssh-agent socket, or wayland/pulseaudio sockets under
`/run/user`.

**Reconfigure:** re-add it explicitly only where a pane genuinely needs it:

```toml
[sandbox]
mounts = ["~/.gitconfig:ro", "~/.gnupg:rw", "/run/user"]
```

Prefer passing the specific secret the pane needs (a `keyring:`/`env:`/`file:`
ref resolved by the broker, or a `[bundle.*] env` entry) over re-mounting the
whole session bus.

## 2. The SSH agent socket is dropped from sealed panes

**What changed:** the `sealed` / `sealed-tunnel` tiers drop `SSH_AUTH_SOCK` from
`env_passthrough` by default. (With #1, the socket is unreachable on every tier
anyway unless `/run/user` is re-mounted.)

**Who is affected:** a sealed-tier pane that used the forwarded agent for
git-over-ssh.

**Reconfigure:** if a sealed pane must use the agent, re-add both the var and the
mount for that scope (`[workspace.<slug>] sandbox_env_passthrough` / a repo
`.thegn.toml`), and mount `/run/user`. `thegn doctor` flags it.

## 3. SSH agent forwarding is OFF by default

**What changed:** `[sandbox.remote] forward_agent` and `[env.<name>.ssh]
forward_agent` now default to `false`. Managed (`ManagedFresh`) and loopback
(`LoopbackTunneled`) instances force forwarding off regardless — an ephemeral
box has no business signing with your agent.

**Who is affected:** a remote worktree / user host that did `git push` (or any
agent-authenticated ssh) using your forwarded local keys.

**Reconfigure:** opt back in per host:

```toml
[sandbox.remote]
forward_agent = true          # this trusted host may use my local agent

# or per named env:
[env.deploybox.ssh]
forward_agent = true
```

## 4. Managed SSH keys default to per-account scope

**What changed:** `[credentials.ssh] managed_key_scope` defaults to
`per-account`. Newly provisioned instances authorize a key **private to their
provider account**, so rotating/retiring one account's key can't affect another
account's instances. Existing VPS/Fly instances recorded in the lifecycle
registry and existing Sprites with a local provision marker retain the historic
shared key rather than silently switching identity.

Pre-ledger Machine0 instances cannot be identified locally. Set
`managed_key_scope = "shared"` before the first post-upgrade connection to such
an instance, then reprovision it under per-account custody. To keep the old
single key for every newly managed remote, leave that setting in place.

**New:** `thegn secret ssh rotate [--account <a>]` performs a managed-provider
rotation (generate → authorize across the scope's live instances → verify an
SSH connection with the replacement → de-authorize old → retire). It rolls
back to the old key only if every old-key restoration succeeds; otherwise the
verified replacement remains canonical and the error identifies the degraded
recovery. New Sprites custody records include the value-free originating
worktree and prove the replacement with OpenSSH over the WSS proxy; older
Sprites records without that context fail closed before any mutation.

## 5. Plaintext tokens in config: warned, still working, migratable

Issue-tracker (`[[issues.issue_accounts]] token`) and GitLab CI
(`[ci.gitlab] token`) fields that hold a **raw pasted token** now warn in
`thegn config validate` (they still resolve). Move them into the keyring:

```
thegn secret migrate           # keyring ← plaintext, config rewritten to the ref
```

Issue-tracker and CI tokens resolve `keyring:` / `env:` / `file:` today and are
typed and migratable — `secret migrate` moves a pasted token into a `0600` file
and rewrites the field to `file:<path>` (resolvable at fetch with no change).
VPN, snapshot-S3, and MCP upstream env values also resolve through the typed
broker, but their arbitrary nested map keys are not auto-rewritten by
`secret migrate`; replace a bare value with an explicit ref manually. Provider
tokens (`api_key_env`) resolve through the keyring-capable broker today. Store
any value off-argv with
`printf %s "$TOKEN" | thegn secret set <account>` and paste the printed ref.
