# THE-29 chunk 2 — done

**Commit subject:** `feat(the-29): expose sessions.fork across control surfaces`

## Implemented

- Added `Verb::ForkSession`, the `sessions.fork` catalog row with write scope
  and `SurfaceSet::ALL`, and the MCP state-tool descriptor/capability.
- Added completion classifications for the fork session, worktree, agent,
  cwd, and provider/harness arguments. Structural booleans need no slots.
  The env-overlay ratchet is unchanged because fork adds no config key.
- Added additive `ForkSpec`, `SessionInfo.forked_from`, and fork/native-id
  fields on `AgentLaunch`, all serde/schema compatible with older payloads.
  The fork request contains intent only; it has no argv, env, prompt, or
  transcript fields.
- Added the default-unimplemented `ControlApi::fork` method, the HTTP
  `POST /v1/sessions/fork` route and generic `API_CALLS` row, the typed client
  method, and the gRPC `ForkSession` RPC/request. gRPC maps the additive
  lineage field and derives its capability coverage from the catalog projection.
- Regenerated `docs/api/control-v1.json` through the control-schema snapshot
  test; no `SURFACE_GAPS` or env-overlay entries were added.

## Tests

- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp just quick thegn-core` — passed.
- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp cargo nextest run -p thegn-core capability completion` — passed (60 tests).
- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp just quick thegn-svc` — passed.
- `XDG_STATE_HOME="$(mktemp -d)" THEGN_UPDATE_SNAPSHOTS=1 RUSTC_WRAPPER= cargo test -p thegn-svc --test control_schema` — passed and regenerated the snapshot.
- Targeted control-schema and route mirror tests — passed.
- Targeted `thegn-core` MCP state tests — passed (24 tests).
- Targeted gRPC capability/frame tests — passed (2 tests), plus the additive
  `SessionInfo.forked_from` mapping test — passed.
- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp cargo check -p thegn-svc --features control-grpc` — passed.

## Unverified

- `just quick thegn-host` was attempted and cannot pass until chunk 3 updates
  six existing struct literals in chunk-3-owned files with defaults for the
  additive `AgentLaunch.fork`, `AgentLaunch.native_session_id`, and
  `SessionInfo.forked_from` fields. Those files were left untouched per the
  chunk boundary.
- A broader `cargo nextest run -p thegn-svc control_schema routes control`
  filter also hit two unrelated existing socket tests that failed with
  `Operation not permitted`; the exact schema and route tests passed.
- Full workspace gates, migrations/live `thegn` invocations, and e2e were not
  run per the chunk dev-loop policy.
