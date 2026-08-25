# Design — event-feed subscriptions

## The Herdr study, condensed

Herdr's socket API (https://herdr.dev/docs/socket-api/): newline-delimited
JSON over a unix socket / named pipe; `{"id","method","params"}` requests
with id-correlated responses; `events.subscribe` takes an array of typed
subscriptions with per-resource filters; `session.snapshot` bootstraps state
and events keep it current; errors are `{"code","message"}`; a schema is
exportable; no auth beyond socket file permissions.

Mapping to thegn: transport and auth stay as they are (HTTP/WS/SSE + gRPC
with scoped tokens is strictly stronger, and the plugin wire is already
NDJSON-with-ids where a line protocol is the right shape). What transfers is
the _event model_: filters, snapshot-bootstrap, visible loss, stable error
codes. Not adopted (recorded so the decision is deliberate):

- **Single-socket JSON-RPC for control calls** — thegn's RPC is HTTP;
  correlation is inherent; a second RPC framing would be a parallel door.
- **`report_metadata` / token maps** (client-contributed display state with
  TTLs) — real value for supervisors, but it is a new _write_ capability and
  belongs behind its own catalog row; deferred, noted as an open question.
- **Atomic `agent.prompt` + wait** — `OpenSpec.agent` + `sessions.wait`
  compose today; an atomic variant is sugar, deferred.
- **Replay/resume** — Herdr does not have it either; the lag signal +
  re-snapshot is the honest contract for an ephemeral broadcast feed.

## Filter semantics

A pure `FeedFilter` in `thegn-core::control_wire` (parsed from query params /
gRPC request fields; unit-tested):

- `kinds`: subset of the frame-kind vocabulary already used by `frame_json`
  (`activity`, `lease`, `pairing`, `sessions`, `exit`, `state`, `lagged`;
  `hello` is always sent). Unknown kind names are rejected with
  `bad_request` — a typo silently filtering everything out would be the
  worst failure mode.
- `session`: keeps only frames keyed to that session (`activity`, `lease`,
  `exit`); un-keyed kinds pass unless excluded by `kinds`.
- Filtering happens in the per-connection pump (WS/SSE/gRPC), never in the
  daemon's broadcast — one broadcast, many views, no new wake source.
- Filters narrow only: they can never grant a frame the token's `read` scope
  would not already see (the whole feed is read-scope; no per-scope frames
  exist today, so this is future-proofing language, not a behavior change).

## Wire evolution: opt-in tags, PROTO_VERSION stays 1

`EventDecoder` fatals on unknown tags (`WireError::UnknownTag` tears the
stream down) — by design, so corruption never limps. New frames therefore
ride behind explicit opt-in: the server sends `State` only when
`snapshot=1` was requested and `Lagged` only when the connection asked for
lag signaling (`lag=signal`, folded into the same query params / request
fields). Old clients never see a new tag; new clients declare themselves.
`Hello` gains `features: ["state","lagged","filters"]` (serde-additive; JSON
`Hello` bodies ignore unknown fields, so old clients decode it unchanged) so
a client can feature-detect before relying on either. `PROTO_VERSION` bumps
only on an _incompatible_ framing change; this is additive, so it stays 1.

Frame shapes:

- `State { sessions: Vec<SessionInfo>, worktrees: Vec<WorktreeInfo> }` —
  compact JSON payload, same wire types as `sessions.list` /
  `worktrees.list`, emitted once after `Hello`. It answers under the same
  `read` scope those verbs require, so it exposes nothing a subscriber could
  not already fetch.
- `Lagged { missed: u64 }` — the count from
  `broadcast::error::RecvError::Lagged(n)`; the client re-snapshots (or
  re-lists) and continues. The pane-attach stream keeps its existing,
  stronger resync (fresh `PaneSnapshot`) — this change touches only the
  monitor feed.

## Error codes

`ControlError` already is the taxonomy; the HTTP adapter's
`{"error": msg}` body gains a sibling `code` field derived from the variant
(`not_found`, `no_scope`, `conflict`, `unimplemented`, `internal`) plus
adapter-level `unauthorized` and `bad_request`. gRPC status codes and plugin
`RpcErrorCode` remain their surfaces' projections of the same enum — one
vocabulary, three spellings, no second policy table. Additive on the wire:
existing clients parsing `error` keep working; the schema snapshot
(`docs/api/control-v1.json`) is regenerated and the snapshot test pins it.
Open question 2 covers whether `internal` messages should stop carrying
`anyhow` detail on the wire.

## The CLI tail

`thegn events tail` attaches to `/v1/events` over the control socket (SSE
JSON internally — one consumer path, no bespoke WS client), applies the same
filter flags, prints human lines by default and NDJSON with `--json` via the
one emitter. It is the cheapest reference client for the new features and
what makes the feed debuggable without writing code. Catalog: the
`events.subscribe` row's surfaces gain `Cli`; `cli_control_caps()` grows;
implemented in the same change so no `SURFACE_GAPS` entry is ever added
(the sibling change's ratchet makes adding one loud).

## Event loop / render / DB

Daemon/svc/CLI side only: no render damage (channel: none), no new wake
source, no polling timeout anywhere; the compositor's own daemon consumption
is untouched (it opts into nothing). No SQLite change, no `user_version`
bump. No new TUI action/keybind — no help-context claim; `thegn events tail`
is documented in CLI help output like other verbs.

## Security

- **No new capability, no scope change**: filters/snapshot/lag all live
  inside `events.subscribe` (read scope); the `State` frame carries only what
  `read` already reads. The CLI surface addition reuses the same verb and
  scope.
- **Filter inputs are caller-controlled**: parsed by a pure, bounded parser
  (fixed vocabulary, session id length-capped like path params); rejects,
  never panics; no regex, no allocation amplification.
- **Error codes leak less, not more**: codes are a closed enum; messages are
  unchanged this change (tightening `internal` messages is open question 2).
- **DoS posture unchanged**: per-connection filtering is O(frames) work the
  pump already did; a client cannot subscribe itself into extra server work
  (narrowing only drops sends).
- **Credentials**: untouched — bearer tokens as today on WS/SSE/gRPC.

## Open questions

1. Client-contributed metadata (Herdr's `report_metadata`/token maps) — a
   supervisor labeling sessions is genuinely useful; needs its own catalog
   row (`sessions.annotate`?) and a write-scope story. Separate proposal.
2. Should `ControlError::Internal` stop carrying `anyhow` chains in HTTP
   bodies (generic wire message + full detail in the audit/tracing log)?
   Today's behavior is unchanged here.
3. Should the gRPC events stream also gain the snapshot/lag features in this
   change, or ride until a consumer exists? Scoped in (the proto fields are
   additive and cheap), but the tasks order HTTP first.
