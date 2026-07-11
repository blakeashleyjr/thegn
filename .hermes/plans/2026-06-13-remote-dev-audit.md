# Remote Development Implementation Audit

## Executive Summary

thegn has a **sophisticated but incomplete** remote development story. Three distinct modes are architected:

1. **SSHFS mode** (`mode=sshfs`) - Mount remote filesystem locally via sshfs
2. **Local client / Remote daemon** (`mode=remote_exec`) - Run thegn client locally, agents on remote
3. **Fully remote** (`mode=remote`) - SSH/mosh into a remote machine and run thegn natively

The architecture is **transport-agnostic**: the `GitLoc` abstraction lets every git/gh operation route through ssh transparently, and the sandbox layer composes mosh/ssh transports around OCI container execs. However, **platform provider integrations are missing** - there's no API-specific support for Codespaces, Modal, E2B, Fly.io, etc. Everything is user-configured SSH targets.

---

## Current Implementation Status

### Implemented Components

#### Core Abstraction: `GitLoc` (`crates/thegn-core/src/remote.rs`)

- **Purpose**: Where a worktree's git data lives - local or remote over ssh
- **Storage**: Remote location persisted as JSON blob in `worktrees.location` column
- **Resolution**: `GitLoc::for_worktree(path)` resolves location from DB; local on miss
- **Operations**: `git_command()`, `gh_command()`, `sh_command()`, `git_with_stdin()` - all route through ssh when remote
- **SSH base**: Multiplexed connection via ControlMaster (`ControlPath=$THEGN_DIR/ssh-%r@%h:%p`, `ControlPersist=300`)

#### Sandbox Layer (`crates/thegn-core/src/sandbox.rs` + `crates/thegn-svc/src/ssh.rs`)

- **`RemoteConfig`** (`config.rs:895-926`):

  ```toml
  [sandbox.remote]
  host = ""           # Empty = local
  port = 22
  transport = "mosh"  # mosh | ssh
  mode = "remote"     # remote | local_exec | sshfs
  remote_dir = "~/thegn-worktrees"
  forward_agent = true
  ```

- **`RemoteMode`** enum (config.rs:143-146):
  - `Remote` = Worktrees live on the remote machine (default)
  - `LocalExec` = Local worktrees, but agents run via ssh
  - `Sshfs` = Mount remote filesystem via sshfs locally

- **`RemoteTransport`** enum (config.rs:137-140):
  - `Mosh` = Mosh preferred for interactive panes (default)
  - `Ssh` = Fallback SSH with TTY

- **`Transport`** enum (sandbox.rs:194-197):
  - `Local` vs `Remote(Remote)` - drives where container lifecycle commands run

- **Backend resolution** (sandbox.rs:579-605):
  - If `GitLoc::Remote` → use ssh's host/port
  - Else if `cfg.remote.is_remote()` → use configured remote host

- **Container lifecycle on remote** (sandbox.rs:793-882, 884-904):
  - `ensure()` creates keep-alive podman/docker container on remote
  - `teardown()` destroys container via ssh
  - `container_status()` probes remote container via ssh
  - `prefetch_image()` runs `image exists`/`pull` on remote

- **Interactive pane entry** (sandbox.rs:1171-1198):
  - `transport_wrap()` composes: `mosh --ssh="ssh -p PORT -A -o ControlMaster..." host -- /bin/sh -lc "<backend_cmd>"`
  - Or `ssh -t host -- /bin/sh -lc "<backend_cmd>"` for SSH transport

- **Available over transport** (sandbox.rs:669-691):
  - For `Transport::Remote`, probes via `ssh host "command -v podman >/dev/null 2>&1"`

#### Agent Launching (`crates/thegn-host/src/agent.rs`)

- `launch_spec()` - Composes final argv from worktree location + sandbox config
- `prepare_sandbox()` - Resolves sandbox, ensures container (or host fallback)
- `compose_spec()` - Builds `LaunchSpec` (argv, cwd, env, backend label, warnings)
- **Key insight**: `backend_label` stored in DB per worktree for persistence

#### SSH Control Plane (`crates/thegn-svc/src/ssh.rs`)

- **Trait**: `RemoteExec` with `exec()` and `home()` methods
- **Fallback**: `CliSsh` wraps subprocess ssh (permanent fallback for ProxyJump/Match hosts)
- **russh path**: Native Rust SSH (not yet implemented, stubbed for Phase 5)

### Roadmap Status (from `tasks.md`)

#### J. Remote access - Section 387-403

| Feature                                | Status | Notes                                                         |
| -------------------------------------- | ------ | ------------------------------------------------------------- |
| 121. SSH attach                        | `~`    | GitLoc exists, but no explicit `thegn ssh-attach` command     |
| 122. Mosh support                      | `~`    | TransportKind::Mosh exists, `transport_wrap` builds mosh argv |
| 123. Tailscale zero-config path        | ` `    | Not started - would be transport layer addition               |
| 124. iroh embedded p2p                 | ` `    | Not started - P2P transport alternative                       |
| 125. iroh hole-punching + relay        | ` `    | Not started                                                   |
| 126. Tunnel stdio agents over iroh/ssh | ` `    | Not started                                                   |
| 128. Remote daemon mode                | `~`    | `mode=remote` exists in config                                |
| 129. Local UI → remote agents          | `~`    | `mode=local_exec` exists in config                            |
| 132. Connection status indicator       | `~`    | Partial - some statusbar widget support                       |
| 133. Reconnect/resume on drop          | ` `    | Not started                                                   |
| 134. Bandwidth-adaptive rendering      | ` `    | Not started                                                   |

#### AB. Container management - Section 684-700

| Feature                            | Status | Notes                                               |
| ---------------------------------- | ------ | --------------------------------------------------- |
| 349. bollard Docker/Podman control | `~`    | Native bollard dep exists, but using subprocess CLI |
| 350. Sandbox per worktree          | `x`    | Fully implemented                                   |
| 351. "4 containers in directory"   | `~`    | Sidebar SANDBOXES panel exists                      |
| 352. Spawn/stop/restart            | `~`    | `ensure()` / `teardown()` implemented               |
| 355. BYO image substitution        | `x`    | `sandbox.image` config                              |
| 356. Resource caps                 | `~`    | `limits.cpu` / `limits.memory` fields exist         |
| 361. Container↔worktree binding    | `x`    | Deterministic naming via `container_name()`         |
| 362. Default-on with --no-sandbox  | `x`    | `backend = "none"` or `enabled = false`             |

---

## Platform Provider Gap Analysis

### Requested Platforms vs Current State

| Provider                                        | Current State | Gap                                                     | Recommendation                                                            |
| ----------------------------------------------- | ------------- | ------------------------------------------------------- | ------------------------------------------------------------------------- |
| **Codespaces** (github.com/features/codespaces) | None          | No API integration. SSH target must be user-configured. | Add `provider = "codespaces"` mode that calls `gh codespace list/connect` |
| **Modal** (modal.com)                           | None          | No integration. User would need to SSH manually.        | Add `provider = "modal"` with `ssh modal proxy ...` spawn                 |
| **E2B** (e2b.dev)                               | None          | No integration.                                         | Investigate E2B Sandbox API → spawn persistent sandbox over WebSocket     |
| **Coder** (coder.com)                           | None          | No integration.                                         | Add `provider = "coder"` using `coder ssh` command                        |
| **Fly.io** (fly.io)                             | None          | No integration.                                         | Add `provider = "fly"` using `fly ssh console` or app SSH gateway         |
| **Coolify** (coolify.io)                        | None          | No integration.                                         | SSH-based - same as custom remote if Fly SSH enabled                      |
| **Railway** (railway.com)                       | None          | No integration.                                         | SSH-based - similar to other providers                                    |
| **Qovery** (qovery.com)                         | None          | No integration.                                         | SSH-based - similar to other providers                                    |
| **Vercel Sandbox** (vercel.com/sandbox)         | None          | No integration.                                         | Check if exposes SSH endpoint                                             |
| **Bunnyshell** (bunnyshell.com)                 | None          | No integration.                                         | SSH-based if exposed                                                      |
| **Together.ai Sandbox** (together.ai/sandbox)   | None          | No integration.                                         | Check API for SSH access                                                  |
| **Okteto** (okteto.com)                         | None          | No integration.                                         | `okteto ssh` command exists - could integrate                             |

### Architecture for Provider Integration

Each provider needs:

1. **Discovery**: List available sandboxes/dev environments
2. **Connection**: Obtain SSH target (host, port, auth)
3. **Lifecycle**: Start/pause/resume/stop the environment
4. **Metadata**: Map thegn worktree path to provider's path

**Pattern for integration** (see `RemoteConfig` + new providers):

```toml
[sandbox.remote]
provider = "codespaces"  # New field
# OR keep using host + add discovery command
discovery_cmd = "gh codespace list --json name,sshUrl"
```

**Trait for provider backends** (to be added in `thegn-core/src/provider.rs`):

```rust
trait RemoteProvider {
    fn discover(&self) -> Vec<RemoteTarget>;  // List available envs
    fn connect(&self, target: &RemoteTarget) -> SshTarget;  // SSH details
    fn lifecycle(&self, target: &RemoteTarget, action: LifecycleCmd) -> Result<()>;
}
```

---

## Three Remote Modes - Detailed Analysis

### Mode 1: SSHFS Mount (`mode = "sshfs"`)

**Current status**: Config exists, implementation incomplete.

**Architecture**:

- Local thegn runs natively
- Worktree directory mounted via sshfs to local path
- Git/PR panel work against local path (filesystem is remote)
- Sandbox applies locally (or could apply on remote)

**Missing pieces**:

- No sshfs auto-mount/daemon management
- No unmount on session close
- No path conflict detection (local path already exists?)

### Mode 2: Local Client / Remote Daemon (`mode = "local_exec"`)

**Current status**: Config exists, partial implementation.

**Architecture**:

- Worktree exists locally
- Interactive pane runs `ssh -t host -- <agent_cmd>`
- Git/gh operations run locally (same repo on both ends)
- Or: git operations run via ssh if worktree path is remote-mirrored

**Missing pieces**:

- No explicit "local worktree, remote command" mode
- Sandbox resolution doesn't distinguish this case clearly
- User must manually sync repo to remote

### Mode 3: Fully Remote (`mode = "remote"`)

**Current status**: Work in progress.

**Architecture**:

- Worktree lives on remote machine
- GitLoc::Remote stores `{host, port, path}`
- Git/gh/shell operations all route over ssh
- Agent panes use mosh/ssh transport wrapping backend

**Missing pieces**:

- No `new-worktree --remote` flow with sshfs fallback
- No workspace creation directly on remote
- No remote worktree path resolution (`remote_dir` config unused)
- No visual distinction in sidebar for remote worktrees
- No remote workspace deletion/cleanup

---

## Gaps and Risks

### High-Priority Gaps

1. **Provider Integration** - Users must manually configure SSH targets for every cloud platform
2. **Remote Worktree Creation** - No flow to create worktrees on remote machines
3. **Remote Workspace Management** - Cannot add repos to remote side from local picker
4. **Connection Resilience** - No reconnect/resume on network drops
5. **Mosh Integration Incomplete** - Only builds argv, no status monitoring

### Medium-Priority Gaps

1. **SSHFS Mode** - Config exists but not wired to actual mount
2. **Native SSH (russh)** - Using subprocess fallback, russh integration pending
3. **Remote Port Forwarding** - For accessing services in remote containers
4. **Remote File Read/Write** - Works for gitdir only, not general files

### Low-Priority Gaps

1. **Bandwidth-adaptive rendering** - Mosh handles roaming, but no quality scaling
2. **Tailscale integration** - Would simplify SSH setup for some users
3. **iroh P2P transport** - Alternative to SSH for direct connections

---

## Phased Roadmap for Expansive Remote Support

### Phase 1: Foundation (Next 2-3 weeks)

**Goal**: Wire existing stubs into working basic remote workflows.

| Task | Description                              | Acceptance Criteria                                                                       |
| ---- | ---------------------------------------- | ----------------------------------------------------------------------------------------- |
| R-1  | Implement `remote_dir` worktree creation | `thegn new-worktree --remote user@host` creates worktree in `~/thegn-worktrees` on remote |
| R-2  | SSHFS mount/unmount lifecycle            | Auto-mount sshfs when `mode=sshfs`; unmount on session end                                |
| R-3  | Remote workspace deletion                | `close-worktree` runs `git worktree remove` on remote via ssh                             |
| R-4  | Connection status widget                 | Statusbar shows "SSH: user@host" or "MOSh: user@host" for remote sessions                 |
| R-5  | Remote worktree UI distinction           | Sidebar shows remote worktrees with different icon/color                                  |

### Phase 2: Provider Integrations (4-6 weeks)

**Goal**: First-class support for major cloud dev platforms.

| Task | Provider          | Description                                                      |
| ---- | ----------------- | ---------------------------------------------------------------- |
| R-6  | GitHub Codespaces | `provider = "codespaces"` auto-discovers via `gh api`            |
| R-7  | Modal             | `provider = "modal"` spawns sandbox via API, SSH via `modal ssh` |
| R-8  | E2B               | `provider = "e2b"` uses E2B SDK for sandbox lifecycle            |
| R-9  | Fly.io            | `provider = "fly"` uses `fly ssh console` for existing apps      |
| R-10 | Okteto            | `provider = "okteto"` integrates with `okteto dev` lifecycle     |

**Acceptance Criteria per Provider**:

- Discovery command lists available environments
- Connection details auto-resolved (no manual host/port)
- Lifecycle commands work (pause/resume/stop)
- Worktrees map to provider environment paths

### Phase 3: Resilience & Advanced (6-8 weeks)

**Goal**: Production-grade remote development.

| Task | Description              |
| ---- | ------------------------ | --------------------------------------------------- |
| R-11 | Reconnect/resume on drop | Detect mosh/SSH disconnect, offer reconnect         |
| R-12 | Native russh backend     | Replace subprocess SSH for direct connections       |
| R-13 | iroh P2P transport       | Alternative transport for direct machine-to-machine |
| R-14 | Port forwarding          | Expose remote container ports locally               |
| R-15 | Bandwidth-adaptive       | Scale rendering quality on slow connections         |

---

## Testing Strategy

### Existing Tests (Verified)

- `remote_roundtrip_and_argv` - SSH argv construction
- `env_command_prefixes_env_remotely_and_sets_it_locally` - Env passthrough over ssh
- `mosh_wraps_backend_over_ssh` - Mosh argv wrapping
- `ssh_transport_uses_tty` - SSH with TTY flag

### Missing Tests (Needed)

- Remote worktree creation (ssh `git worktree add` on remote)
- Remote sandbox `ensure()`/`teardown()` over ssh
- Provider discovery command parsing
- SSHFS mount/unmount on workspace switch
- Connection drop/reconnect handling

---

## Configuration Examples

### Remote Worktree (Manual SSH)

```toml
# ~/.thegn/config.toml
[sandbox.remote]
host = "user@devbox"
port = 22
transport = "mosh"
mode = "remote"
remote_dir = "~/projects"

# Per-repo override
# .thegn.toml in repo root
[sandbox.remote]
host = "user@gpu-box"
mode = "local_exec"  # Run on remote, but repo synced
```

### Provider Integration (Codespaces)

```toml
[sandbox.remote]
provider = "codespaces"
# discovery_cmd auto-set
# host/port resolved dynamically from gh api
```

### Provider Integration (Modal)

```toml
[sandbox.remote]
provider = "modal"
image = "nvidia/cuda:12.4-devel"  # Modal sandbox image
gpu = "all"
```

---

## Risks & Tradeoffs

1. **SSHFS UX** - Mounting remote filesystems can be slow/unreliable; may need progress UI
2. **Provider API churn** - Cloud dev platforms evolve rapidly; integrations may need updates
3. **Auth complexity** - Each provider has different auth (SSH keys, API tokens, OAuth)
4. **State sync** - If repo diverges between local/remote without sync, git operations break
5. **Network assumptions** - Current code assumes reasonably stable connection; mobile/roaming needs more resilience
