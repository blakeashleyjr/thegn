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
account's instances. **Existing shared-key instances keep working** — the scope
only affects instances provisioned after the change.

**Who is affected:** nobody's existing fleet breaks. If you want the old single
key for every managed remote, set `managed_key_scope = "shared"`.

**New:** `thegn secret ssh rotate [--account <a>]` reports the rotation plan
(generate → authorize across the scope's live instances → verify → de-authorize
old → retire; partial failure leaves both keys live).

## 5. Plaintext tokens in config: warned, still working, migratable

Issue-tracker (`[[issues.issue_accounts]] token`) and GitLab CI
(`[ci.gitlab] token`) fields that hold a **raw pasted token** now warn in
`thegn config validate` (they still resolve). Move them into the keyring:

```
thegn secret migrate           # keyring ← plaintext, config rewritten to the ref
```

Issue-tracker and CI tokens resolve `env:` / `file:` today (as before) and are
now typed and migratable — `secret migrate` moves a pasted token into a `0600`
file and rewrites the field to `file:<path>` (resolvable at fetch with no
change). `keyring:` for issue/CI tokens at fetch time lands with the svc
resolver-injection follow-up (task 2.3); the ref vocabulary already parses it.
Provider tokens (`api_key_env`) resolve through the keyring-capable broker
today. Store any value off-argv with
`printf %s "$TOKEN" | thegn secret set <account>` and paste the printed ref.
