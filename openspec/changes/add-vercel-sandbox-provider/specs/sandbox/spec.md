# Sandbox

## ADDED Requirements

### Requirement: Vercel is a registered remote sandbox kind

thegn SHALL accept `provider = "vercel"` with required `team_id` and
`project_id`, and MUST report ssh as unreached, checkpoints as unsupported and
`host_cache` and `connect = "ssh"` as refused for it.

#### Scenario: Unsupported options are refused

- **WHEN** a vercel environment sets `host_cache = true`
- **THEN** provider admission refuses it

### Requirement: Credentials never enter a Vercel sandbox

For a credential-free kind thegn MUST seed the repo as a host-built git bundle,
MUST NOT forward a git token or origin URL, and MUST NOT run the agent-config,
dotfiles, atuin or home-closure steps. The create body MUST NOT carry secrets.

#### Scenario: Provisioning strips credential steps

- **WHEN** a plan for a vercel environment contains a dotfiles step
- **THEN** the step is removed from the plan and the applier refuses it if present

### Requirement: Keep-alive only while a bridge is live

thegn SHALL extend a Vercel session timeout only from a live `vercel-exec`
bridge process, capped by the lifetime ceiling, and MUST NOT run any timer or
wake source otherwise. A 4xx on extend SHALL stop extending with one warning.

#### Scenario: Idle sandbox stops

- **WHEN** no bridge is live
- **THEN** the session times out and stops and thegn performs no network call

### Requirement: Reads never resume a stopped sandbox

Capture-mode exec MUST NOT resume a stopped sandbox; it SHALL exit 75 with
"vercel sandbox stopped". Only an interactive TTY session resumes.

#### Scenario: Chrome read on a stopped sandbox

- **WHEN** a git or fs read runs against a stopped sandbox
- **THEN** it exits 75 and no compute is started

### Requirement: Caps and lifetime stop-then-delete

`max_instances` SHALL default to 2 and `max_lifetime_secs` to 24 hours. At the
ceiling the reaper MUST stop the session and mark it expired, and SHALL delete
the sandbox only after the 7 day grace period or when its worktree is gone.

#### Scenario: Expired sandbox keeps its work

- **WHEN** a sandbox passes its lifetime ceiling
- **THEN** it is stopped, not deleted, and `thegn doctor` lists it with a `thegn env pull` hint
