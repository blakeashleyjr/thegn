# THE-49 — agent memory evaluation and decision

Status: accepted evaluation. Decision: no new thegn-owned memory system in this
change. Deliverable: this decision record plus one documentation chunk.

## Decision

“Memory” for thegn means durable, attributable context that a later agent can
read across worktrees and sessions: dispatch state and handoffs, git-tracked
pipeline documents, tracker context, and local cache state. It does not mean a
model, embeddings service, transcript database, or an agent-only lifecycle
owned by the shell.

Thegn already has enough durable substrate for the current workflow. It should
remain the AI-free shell described by `CLAUDE.md`: harness memory files and
optional tools such as memex belong to Claude Code/Codex/the user's harness;
thegn may expose its own existing state, but does not become the memory engine.

No THE-49 implementation is authorized here: no new database/table or FTS
index, no `memory.*` catalog row, no MCP-proxy wiring, no background indexer,
no new config keys, and no network activity. This is intentionally a docs-only
close-out with follow-ups below.

## What “memory” already exists

The following is the current-branch evidence map. “Read path” means an agent or
supervisor can use it today; it does not imply that every source is searchable
through one API.

| Source                           | Durable owner and current read path                                                                                                                                                                                                                                                                                                                                                                                                                          | What it provides / limits                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Dispatch roster                  | SQLite `agent_dispatches`; `thegn dispatch list` is documented as a local roster read in [`cmd/dispatch.rs`](../../../../crates/thegn-host/src/cmd/dispatch.rs#L1-L21). The catalog has `dispatches.list` at [`capability.rs`](../../../../crates/thegn-core/src/capability.rs#L578-L582).                                                                                                                                                                   | Issue, worktree, agent, status, stage, parent, session and artifact pointers. It is orchestration memory, not an artifact store.                                                                                                                                                                                                                                                                                                                                                                                              |
| Dispatch report (`THE-88`)       | `thegn dispatch report <id>` writes the report and `dispatch status` reads it [`cmd/dispatch.rs`](../../../../crates/thegn-host/src/cmd/dispatch.rs#L135-L181); storage is a last-write-wins update on the roster row [`db_dispatch.rs`](../../../../crates/thegn-core/src/db_dispatch.rs#L11-L25). The row model says the report is what the Lead reads and the artifact remains in git [`issue.rs`](../../../../crates/thegn-core/src/issue.rs#L274-L281). | A structured handoff summary (verdict, commits, unverified items, findings, next hints), capped at 16,384 characters by pure core policy [`pipeline_report.rs`](../../../../crates/thegn-core/src/pipeline_report.rs#L9-L16). `dispatches.report` and its status read are currently CLI-only [`capability.rs`](../../../../crates/thegn-core/src/capability.rs#L614-L630).                                                                                                                                                    |
| Dispatch note                    | `thegn dispatch note <id>` appends to `agent_dispatch_notes`; `dispatch status <id> --since` is its bounded on-demand read [`cmd/dispatch.rs`](../../../../crates/thegn-host/src/cmd/dispatch.rs#L150-L181), implemented with `since`/limit ordering in [`db_dispatch.rs`](../../../../crates/thegn-core/src/db_dispatch.rs#L28-L74).                                                                                                                        | Short progress context, capped at 4,096 characters by pure core policy [`pipeline_report.rs`](../../../../crates/thegn-core/src/pipeline_report.rs#L15-L16). It is deliberately separate from the daemon retry note [`issue.rs`](../../../../crates/thegn-core/src/issue.rs#L258-L265) and is currently CLI-only [`capability.rs`](../../../../crates/thegn-core/src/capability.rs#L620-L630).                                                                                                                                |
| Pipeline artifacts               | Files under `.thegn/pipeline/<ISSUE>/...` are referenced by roster rows; the row explicitly stores a pointer, never the payload, and git is the intended source of truth [`issue.rs`](../../../../crates/thegn-core/src/issue.rs#L252-L257). `dispatch verify` requires the artifact to exist and its path to be in git's index [`pipeline_run.rs`](../../../../crates/thegn-core/src/pipeline_run.rs#L115-L155).                                            | The full architect/coder/reviewer handoff, inspectable from the recorded worktree. The current gate does **not** prove that the bytes being read are in `HEAD`: a newly staged path or a tracked file modified after commit can pass because dirty state is reported but non-blocking [`dispatch.rs`](../../../../crates/thegn-host/src/cmd/dispatch.rs#L629-L648). There is no global artifact search/read capability today; callers must resolve the recorded worktree and path.                                            |
| Linear comments and issue detail | `IssueDetail` carries comments [`issue.rs`](../../../../crates/thegn-core/src/issue.rs#L189-L205). The Linear provider fetches and maps comments in `issues.get` [`linear.rs`](../../../../crates/thegn-svc/src/issue/linear.rs#L451-L474); `issues.get` is a catalog row for the existing control surfaces [`capability.rs`](../../../../crates/thegn-core/src/capability.rs#L560-L575).                                                                    | Live tracker context, including comments, when the configured provider is available. It is network/provider-owned, not an offline memory log. The cache's `issue_cache` stores `Vec<Issue>` list payloads, not `IssueDetail` comments [`db.rs`](../../../../crates/thegn-core/src/db.rs#L620-L635).                                                                                                                                                                                                                           |
| Harness session summaries        | `thegn agent sessions` is the CLI read path [`cmd/agent.rs`](../../../../crates/thegn-host/src/cmd/agent.rs#L1-L13), backed by a bounded, read-on-demand scan of configured local harness stores [`sessions.rs`](../../../../crates/thegn-svc/src/sessions.rs#L1-L11). The `agent.sessions` catalog row describes harness/id/worktree/mtime/summary [`capability.rs`](../../../../crates/thegn-core/src/capability.rs#L349-L355).                            | A credential-free one-line first-user-prompt summary and worktree link; at most 500 newest files per discovery pass [`sessions.rs`](../../../../crates/thegn-svc/src/sessions.rs#L21-L24) [`sessions.rs`](../../../../crates/thegn-svc/src/sessions.rs#L78-L114). The summary is user content and can be sensitive even though credential material is excluded. An unfiltered call scans every configured harness store; callers needing isolation must pass the exact worktree filter. It does not return transcript bodies. |
| Existing SQLite cache            | `$XDG_STATE_HOME/thegn/thegn.db` is a WAL, versioned cache/resurrection layer, not truth [`docs/ARCHITECTURE.md`](../../../../docs/ARCHITECTURE.md#L230-L237). `CacheStore` covers issue, PR, CI, diff, commit, test and LOC snapshots [`store/cache.rs`](../../../../crates/thegn-core/src/store/cache.rs#L14-L107); issue hydration writes best-effort cache rows [`hydrate_tracker.rs`](../../../../crates/thegn-host/src/hydrate_tracker.rs#L88-L121).   | Fast local context for UI/state resurrection. Keys are repo/worktree/account scoped in individual tables, but there is no cross-source document model and no FTS5 search index in the current dependency/code inventory (`rusqlite` is the SQLite dependency [`Cargo.toml`](../../../../Cargo.toml#L45-L55)).                                                                                                                                                                                                                 |

The practical existing “memory loop” is therefore: a supervisor reads the
roster, reads the report/notes, follows an artifact pointer into the worker's
git worktree, and optionally asks `issues.get` or `agent.sessions` for tracker
or session context. That is useful memory without pretending all of those
sources have the same authority, freshness, privacy, or partition key.
Every text-bearing source is untrusted operator-, provider-, or agent-authored
content: control-character stripping on reports/notes prevents terminal
injection, not semantic prompt injection. A reader must preserve attribution,
present retrieved prose as quoted data rather than instructions, and never
execute commands, links, or tool requests merely because retrieved content
asks it to.

## Prior-art evaluation

The issue's referenced [memex](https://github.com/nicosuave/memex) is a good
optional harness tool. Its current README describes local BM25 transcript
search, opt-in local embeddings, and a shared skill workflow. It indexes agent
CLI transcripts, not thegn's git artifacts, Linear detail, or thegn SQLite
rows. Its optional continuous index service and SSH multi-machine federation
also make it the wrong thing to embed behind a no-network,
no-new-wake-source shell rule. Install/use it at the harness layer when
transcript search is wanted; do not vendor it, invoke it from the compositor,
or make thegn's state depend on it.

| Option                                               | Decision                     | Reason                                                                                                                                                                                                                                                            |
| ---------------------------------------------------- | ---------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Embed memex or another memory vendor                 | Reject                       | Violates the AI-free ownership boundary, duplicates harness transcript ownership, adds a moving vendor/runtime dependency, and cannot natively index thegn's mixed sources.                                                                                       |
| Wrap memex as a thegn provider seam                  | Defer/reject for THE-49      | A provider seam is appropriate for a substitutable thegn backend, but this would turn an optional agent utility into a first-class thegn dependency without a current product requirement. No vendor-specific seam or catalog row is justified.                   |
| Add a plain per-workspace Markdown store             | Reject for now               | Duplicates git-backed artifacts and creates competing authority, retention, writes, and privacy rules. A user can already keep notes in the worktree and hand them off by artifact pointer.                                                                       |
| Add SQLite FTS5 to the existing cache                | Defer as a bounded follow-up | It could make local retrieval fast, but it would require an explicit source allowlist, profile/workspace partition key, stale/index rebuild policy, and schema migration. The current cache is intentionally TTL/read-through and no FTS5 substrate exists today. |
| Leave memory to the harness and existing thegn reads | Accept                       | Satisfies optionality and the current AI-free architecture while preserving thegn's durable orchestration and git/cache boundaries.                                                                                                                               |

## Existing MCP proxy draft: verify, prune, do not extend

`openspec/changes/add-mcp-proxy-hub/` was mapped to this issue, but it is a
stale, unarchived design record and is not the authority for THE-49.
Its proposal makes memory a curated MCP preset riding a new proxy
[`proposal.md`](../../../../openspec/changes/add-mcp-proxy-hub/proposal.md#L24-L31)
and specifies a large hub/supervisor/credential/wiring change
[`proposal.md`](../../../../openspec/changes/add-mcp-proxy-hub/proposal.md#L33-L83).
The current branch already contains commit `8f5eb345` (“MCP proxy hub”) and a
smaller implementation in `thegn-core/src/mcp/proxy/` and
`thegn-host/src/mcp_proxy/`. The current host module explicitly calls its v1
path an in-process standalone hub and says the draft's daemon-shared multiplex
is still only a follow-up
[`crates/thegn-host/src/mcp_proxy/mod.rs`](../../../../crates/thegn-host/src/mcp_proxy/mod.rs#L7-L20).

That landed code is not native thegn memory. Its curated `memory-graph` and
`memory-mem0` entries are optional external MCP references
[`presets.rs`](../../../../crates/thegn-core/src/mcp/presets.rs#L38-L80), and
the proxy's own pure policy is already implemented
[`mcp/proxy/mod.rs`](../../../../crates/thegn-core/src/mcp/proxy/mod.rs#L1-L16).
THE-49 adds no proxy work, no preset, and no dependency on that draft. In
particular, do not revive the draft's daemon-owned proxy multiplex, new
credential/wiring scope, health lifecycle, or vendor-specific memory choices.
The old THE-49 attributions in `tasks.md` and the landed preset comments record
that prior decision; they do not authorize more proxy or memory work here.

## Future follow-up contract (not part of this change)

Only if agents demonstrate that the existing read paths are insufficient,
open a fresh small change for one read-only `memory.search` capability. Its
non-negotiable shape is:

1. Optional and disabled by default; no network, no vendor binary, no MCP
   proxy dependency, and no new daemon/compositor wake source. Search is
   explicit/on-demand or piggybacks an already-running read path.
2. Partition every result by active profile + workspace (and worktree when the
   source is worktree-local). Never merge two workspaces merely because they
   share a repo name. Unresolvable context withholds the result/source rather
   than guessing.
3. Index only an explicit local allowlist: committed `.thegn/pipeline` text
   read from a named git commit (not the mutable worktree or index),
   bounded dispatch reports/notes, and already-local cache payloads. Linear
   comments are searchable only when present in an existing local cache; the
   search path must not fetch the network. Transcript bodies remain harness
   owned unless separately opted in.
4. Keep tokenization, normalized terms, deterministic ranking, stable
   tie-breaks, limits, and source attribution in a new substrate-free
   `thegn-core` module. Unit-test ranking order, ties, limits, attribution, and
   profile/workspace isolation. Filesystem, SQLite, and any provider reads
   stay at host/service edges; one unavailable or malformed source degrades to
   no results from that source plus a diagnostic, without failing other local
   sources.
5. Expose it only as a row in `thegn_core::capability::CATALOG` with read
   scope. Add every required surface projection and its tests together:
   control schema, route/API mirror, MCP/plugin coverage, and the relevant
   surface-gap/help ratchets. Do not bolt on a special MCP-only tool.
6. If FTS5 is selected, treat it as a cache/index with an additive,
   ladder-tested `user_version` migration, bounded rebuild, and stale-index
   fallback. If Markdown is selected, git remains authoritative and the
   index remains disposable. In either case, document every config key in
   `config/config.toml.example` and cover env overlays; prefer no new config
   key until a real user workflow requires one.
7. Treat every indexed byte as untrusted content. Preserve source plus
   commit/row attribution, keep content structurally separate from instruction
   channels in every projection, never auto-execute embedded commands, links,
   or tool calls, and never inject a result as system/developer authority.
   Test that adversarial prompt-like text remains inert and attributed;
   control-character filtering alone is not this defense.

This contract is a gate for a future implementation, not a design license for
this docs-only issue. The conclusion for THE-49 is: **no new system**.

## Ratchets and scope

This change adds no Rust, config, capability, help, schema, provider, or
platform code. Therefore no env-overlay, completion-slot, control-schema,
surface-gap, help, provider, platform, or DB migration ratchet changes are
permitted. The only touched deliverables are this record and the independent
chunk specification below. No `thegn` invocation is needed; if one is used for
verification, set `XDG_STATE_HOME` to a temporary directory and never touch a
live state DB.
