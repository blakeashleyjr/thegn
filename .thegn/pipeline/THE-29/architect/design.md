# THE-29 — fork existing sessions

## Decision

Implement `sessions.fork` as a new, non-streaming control capability. A fork
always creates a new PTY/session; it never clones a process, emulator, or
scrollback into the child. The daemon/host owns the live spawn and PTY
snapshot, while `thegn-core` owns the vendor-neutral fork policy, harness
capability contract, command plan, validation, and credential-free lineage
record.

The implementation is split into three serial, file-disjoint coder chunks:

1. core policy, harness seam, and cache record;
2. catalog, completion contract, service/control wire, and contract snapshots;
3. daemon spawn, CLI/MCP/UI placement, and documentation.

This keeps the existing `thegn-host → thegn-svc → thegn-core` dependency
direction. It also keeps vendor syntax in the harness implementations and
keeps PTY, filesystem, git, and async work out of core.

## Verified current seams

The openspec draft was read first as required. It is a useful behavioral seed,
but several statements are stale on this branch:

| Draft claim                                                                         | Current code / consequence                                                                                                                                                                                                                                                                                                                                       |
| ----------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| “Today none of this exists” (`openspec/changes/add-session-fork/proposal.md:32-33`) | The daemon/control/session substrate already exists: `SessionInfo`, `OpenSpec`, `AgentLaunch`, and `ControlApi::open` are in `crates/thegn-svc/src/control/mod.rs:51-187,544-625`; live session ownership is `crates/thegn-host/src/daemon/service.rs:31-84`. Add a fork operation beside those seams.                                                           |
| No agent layer (`proposal.md:7-11`)                                                 | The repository’s historical AI-free note is still in `CLAUDE.md:19-28`, but the current harness and agent orchestration code are real: `crates/thegn-core/src/harness.rs:1-29`, `209-279`, and `crates/thegn-core/src/agent_task.rs:595-772`. THE-29 explicitly asks for these seams, so this design uses them without making the shell depend on a model layer. |
| Generic agent re-resolution is sufficient (`proposal.md:41-46`)                     | It is unsafe as a conversation fork because a cold launch has no native conversation id. An agent fork must use the selected harness’s optional fork operation and a validated native id; an agent without one returns a capability error and points to `thegn agent sessions`. No vendor file format is added to core.                                          |
| Fork is not MCP/plugin v1 (`proposal.md:72-80`, `design.md:115-118`)                | Binding addenda require the same capability catalog to project control API, CLI, MCP, and plugin. `sessions.fork` is therefore `SurfaceSet::ALL`, subject to the existing scope/auth gates.                                                                                                                                                                      |
| No database recipe/record (`design.md:28-34,81-83`)                                 | Recipes remain memory-only, but the binding explicitly requires a fork record in the DB cache. Store only credential-free lineage metadata; never argv, env, prompts, transcript bytes, or vendor paths.                                                                                                                                                         |
| `ForkSpec { session, cwd, worktree, scrollback, tab }` (`tasks.md:7-9`)             | Retain these options, but make the source typed: a live daemon session or a recorded harness session. `--fork-worktree` remains a host-side two-step composition, not a daemon recipe field.                                                                                                                                                                     |

The existing harness seam already has the required shape: object-safe trait,
no async, optional operations paired with capability bits, and a closed
registry (`harness.rs:14-29,38-99,209-279`). `resume_command` is already
vendor-owned (`harness.rs:251-255,359-360,417-424`); fork must follow the same
pattern rather than adding a vendor match to daemon code.

The control catalog is also already authoritative: `Verb` and scope live in
`crates/thegn-core/src/control.rs:237-580`, catalog rows in
`crates/thegn-core/src/capability.rs:183-252`, and the architecture requires
all HTTP/gRPC/CLI/MCP/plugin surfaces to project it
(`docs/ARCHITECTURE.md:151-197`). The route table and generic API spine are
`crates/thegn-svc/src/control/routes.rs:29-68,142-201`.

The requested resume seams are distinct and remain distinct: `agent.sessions`
is the existing discovery capability (`crates/thegn-host/src/cmd/agent.rs:9-10,
29-50,157-176` and `crates/thegn-core/src/capability.rs:351`), while
`thegn session open --resume-work` is THE-86 pipeline-row relaunch logic
(`crates/thegn-host/src/cmd/session.rs:338-360,406-414`, `docs/cli.md:21-23`).
THE-29 adds native conversation fork alongside them; it must not reinterpret
`--resume-work` or duplicate the discovery scan.

## Semantics

### Sources and honesty

`ForkRequest` has one of two source kinds:

- `DaemonSession { id }`: a live daemon session. The daemon retained its
  resolved raw launch recipe in memory, so core can plan a new launch from the
  same argv/cwd/env. The recipe is never serialized or persisted.
- `HarnessSession { harness, id, agent, worktree }`: a credential-free row from
  `agent.sessions`. Core validates the harness id, native session id, and
  `HarnessCaps::FORK`, then asks that harness for its fork command. The host
  supplies the current configured agent/sandbox/credential composition. A
  recorded session whose harness has no fork operation returns `reserved` /
  unsupported; it does not fall back to a guessed command.

For a daemon session originally opened through `AgentLaunch`, the in-memory
launch metadata may carry a native id when the launch was itself a resume/fork.
Only then can a daemon-source fork use the harness fork operation. A cold
agent launch with no native id is refused as “native session id unavailable”;
the user can choose a concrete row from `thegn agent sessions`. This avoids
pretending that replaying an agent executable is a conversation fork.

Every successful operation produces a new daemon id and process. The source
is never paused, signalled, resized, or attached as a side effect. `SessionInfo`
gets additive `forked_from: Option<String>`; for a daemon source it is the
parent daemon id, and for a native source it is the stable display form
`<harness>:<native-id>`. The child’s environment receives
`THEGN_FORKED_FROM=<same display source>` in addition to the existing identity
variables. Existing identity overwrite behavior is in
`crates/thegn-host/src/daemon/service.rs:86-103`.

### Pure core plan

Add a small `thegn_core::session_fork` module, not more branches in
`agent_task.rs` or `db.rs`. It contains:

- typed source/options and bounded validation (reuse `harness::session_id_ok`);
- a `ForkPlan` for raw recipe replay versus a harness operation;
- the policy that `FORK` requires a non-empty native id and a harness that
  advertises `FORK`;
- deterministic argv/identity-environment composition inputs, with the
  harness operation supplying only its own vendor command syntax;
- `ForkRecord`, containing child id, source kind/id, optional harness,
  worktree, and timestamps, but no secret or process recipe.

Raw argv is copied as data and the daemon re-applies the cap with
`already_capped = false`. Agent launches are resolved through the existing
`daemon/agent_open.rs` path at spawn time so current config and credentials are
used; the fork flag/native id is passed to the harness seam, never interpreted
by generic core or daemon string matches. The fork command is an argv/command
plan returned by `Harness::fork_command`; Claude’s implementation may produce
`claude --resume <quoted-id> --fork-session`, Codex may use its supported
`resume` form, and Pi/other harnesses remain reserved until their behavior is
verified. These strings belong only in `harness.rs` implementations and their
tests.

### Daemon operation

`DaemonService::fork` is the only spawn owner:

1. authenticate and scope-check `sessions.fork`;
2. resolve a live source (a tombstone/dead id returns `ControlError::Conflict`
   or the existing not-live error, explicitly naming `sessions.open` as the
   cold-start alternative);
3. request the actor’s bounded history tail through a oneshot only when
   `scrollback` was requested;
4. use core’s plan, fresh agent resolution, sandbox/cap wrapper, and existing
   open/registration path to spawn a new PTY;
5. apply `THEGN_SESSION_ID`, `THEGN_CONTROL_SOCKET`, and
   `THEGN_FORKED_FROM`, and optionally write a 0600 plain-text handoff file at
   `$XDG_STATE_HOME/thegn/forks/<child-id>.txt` with
   `THEGN_FORK_SCROLLBACK` in the child env;
6. insert the credential-free `ForkRecord` as best-effort cache state and
   return only `SessionInfo`.

The scrollback file is context for the child, not emulator history. The child
screen contains only bytes the child writes. Cleanup is best-effort on child
exit; failure is logged and does not block tombstone publication. The recipe
is held on the live `SessionEntry` only, never on `SessionMeta`, tombstones, a
control response, or the DB. The current tombstone shape confirms the right
boundary (`crates/thegn-host/src/daemon/tombstone.rs:49-78`), and the actor
already owns the bounded history ring/snapshot (`daemon/session.rs:53-103,
707-735,1104-1128`).

The DB migration is additive v62 after the current v61
(`crates/thegn-core/src/db.rs:129-136`). The store seam belongs in a new
`store/session_fork.rs` module and the migration/verification in
`db_migrate.rs`, following the extracted-store rule in
`crates/thegn-core/src/store/mod.rs:1-45`. Cache loss or an old schema merely
loses lineage history; it cannot resurrect a credential-bearing recipe.

### Placement and worktree composition

The wire request carries final `cwd`, `worktree`, `adopt`, and `tab` placement
intent. A normal CLI/UI fork sets `adopt` and uses the existing adopt/graft
path. Extend `AdoptIntent` and its pure planner rather than inventing a second
pane attachment path: the current intent payload is
`crates/thegn-core/src/models.rs:49-74`, the planner/graft is
`crates/thegn-host/src/handlers/adopt.rs:43-124,230-335`, and the compositor
drain is `crates/thegn-host/src/run.rs:9540-9565`. `tab` selects a new tab;
otherwise the fork is grafted beside the source/current target. A headless
client simply leaves the new daemon session listed.

`--fork-worktree` is host/CLI composition: create the new worktree using the
existing control operation, remap a source cwd relative to the old worktree,
then call `sessions.fork` with the final paths. Creation failure leaves the
source untouched. If fork fails after creation, report the surviving worktree;
do not delete it implicitly. git remains source of truth and the DB only caches
the resulting lineage. This follows the existing asynchronous worktree path in
`crates/thegn-host/src/daemon/service.rs:1314-1385`.

No fork work is added to the UI render loop. Control calls, PTY snapshotting,
DB writes, and worktree operations stay off-loop; placement arrives through the
existing intent/channel+waker path. This preserves the 0%-idle invariant in
`CLAUDE.md:39-43` and `docs/ARCHITECTURE.md:54-84`.

## Surface contract

Add exactly one catalog row, `sessions.fork`, mapped to `Verb::ForkSession`:

- same write scope as `sessions.open`;
- non-streaming;
- `SurfaceSet::ALL` (HTTP, gRPC, CLI, MCP, plugin generic call);
- summary says “fork a live daemon or recorded harness session” and never
  exposes argv/env or vendor file paths.

The service adds `ControlApi::fork` with an unimplemented default, a typed
`ForkSpec`, additive `SessionInfo.forked_from`, HTTP route
`POST /v1/sessions/fork`, gRPC `ForkSession`, and the generic `API_CALLS` row.
The control schema snapshot must be regenerated, not hand-authored. `agent.sessions`
remains the discovery capability and is the source for selecting a native
recorded id; it is not duplicated or renamed.

CLI syntax is:

```text
thegn session fork <id> [--harness <id>] [--agent <name>]
  [--cwd <dir>] [--worktree <dir>] [--scrollback]
  [--fork-worktree] [--tab] [--json]
```

Without `--harness`, `<id>` is a live daemon id. With it, `<id>` is a native
id discovered by `thegn agent sessions`, and `--agent`/`--worktree` resolve the
launch context when the discovery row is not sufficient. `--fork-worktree` is
not sent as a generic daemon recipe option.

MCP exposes the same operation as the catalog tool `sessions.fork` with flat,
scope-checked arguments (`session`, optional `harness`, `agent`, `cwd`,
`worktree`, `scrollback`, `tab`, `adopt`). No raw `argv` or arbitrary `env` is
added to this new tool. The plugin host uses the generic routed capability, so
there is no second plugin-specific implementation.

Add the `fork-session` action to `ACTION_SPECS`, claim it in
`docs/help/daemon-and-sessions.md`, and route it through a new small handler.
The pane action must refuse a non-daemon/non-session pane with a clear status;
it must not synchronously inspect the DB or invoke git on the event loop.

## Ratchets, config, and tests

There is no new TOML/config key. Fork behavior is an explicit control/CLI
operation and harness capability. Therefore do not invent `[fork]` or
`agents.*.fork` configuration. Verify the unchanged env-overlay contract and
document the no-config-key/capability behavior in the configuration/help
surface. If a coder changes an existing key, it must update
`config/config.toml.example`, `docs/help/configuration.md`, and the env overlay
ratchet in the same chunk; the intended implementation adds no such key.

Required same-change gates:

- core harness caps⇔ops and fork-policy unit tests, including unsupported
  `reserved`, invalid ids, raw-plan identity overwrite, and no recipe/secret in
  `ForkRecord`;
- DB ladder tests from v61 to v62 and cache-store round trips;
- catalog exact-row/scope/surface tests and `SURFACE_GAPS` with no new excuse;
- completion catalog slot for `session fork` plus classification of every new
  value-taking argument; update `test/completion-slot-ratchet.txt` only if the
  generated ratchet requires it, otherwise record that all new values are
  classified as `Session`, `Worktree`, `Agent`, `Structural`, or reserved
  freeform; this ratchet and the control-schema snapshot land in the same
  coder chunk;
- regenerated `docs/api/control-v1.json` and control-schema snapshot;
- help/action/prose/context ratchets and CLI help drift tests;
- `cargo nextest` unit tests for daemon liveness/new-id/new-pid behavior,
  identity overwrite, scrollback permissions/cleanup, dead-session refusal,
  placement, and worktree failure domains.

Do not run a live `thegn` command or migration against the worktree’s normal
state DB. Any smoke/CLI invocation must set `XDG_STATE_HOME` to a fresh temp
directory. No e2e or full-workspace build is part of these coder chunks.

## Pruned draft items

Keep from the draft: new-process honesty, live-only recipe lifetime, optional
plain-text scrollback handoff, lineage env/metadata, adopt-based placement,
worktree creation before fork, no implicit rollback, daemon-disabled error, and
fixed bounded scrollback. Cut or change: “AI layer excised” as a reason to omit
the current harness seam; generic agent cold re-resolution as a conversation
fork; no DB record; and MCP/plugin exclusion. Also do not implement the draft’s
future `apply_layout` or a generic per-agent fork template: neither is present
at the current seam and both would create a second policy surface.
