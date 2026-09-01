# THE-34 chunk 1 — stable control error codes

## Scope

Make control errors machine-readable without changing auth, scopes, status
codes, or prose messages. This chunk is first and is serial with chunks 2 and
3 because it owns the initial schema machinery and changes the HTTP/client
files that chunk 2 extends.

## Exact files touched

- `crates/thegn-core/src/control_error.rs` (new pure error-code enum and tests)
- `crates/thegn-core/src/lib.rs` (module declaration)
- `crates/thegn-svc/src/control/mod.rs` (`ControlError::code`, public error body)
- `crates/thegn-svc/src/control/http.rs` (one structured error serializer and
  adapter error mappings)
- `crates/thegn-svc/src/control/client.rs` (optional code parsing/accessor)
- `crates/thegn-svc/tests/control_schema.rs` (include `ErrorBody`)
- `docs/api/control-v1.json` (generated snapshot)
- `docs/superpowers/specs/control-api.md` (error envelope documentation)

Do not touch `config/config.toml.example`: no config key is introduced. Do
not change `test/surface-gaps-ratchet.txt` or
`test/completion-slot-ratchet.txt` in this chunk.

## Approach

1. Add a serde/schemars-compatible closed `ControlErrorCode` in core. Keep the
   enum independent of HTTP, gRPC, anyhow, and plugin types. Add pure tests for
   stable ids and exhaustive display/serialization.
2. Map the existing `ControlError` variants once. Add `ErrorBody { error, code
}` and route all HTTP `ControlError`, auth, and validation failures through it.
   Preserve current status and message behavior. Do not expose anyhow chains or
   introduce a second policy table.
3. Parse `code` optionally in `ControlRequestError`; old servers that only send
   `error` remain readable. Keep gRPC/MCP/plugin wire shapes unchanged and add
   focused tests that their existing mappings still compile/behave.
4. Add `ErrorBody` to the schema generator and regenerate only with
   `THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema`.
   Update the control-plane spec with the additive envelope and compatibility
   rule.

## Dependency/overlap

Serial prerequisite for chunk 2. It overlaps chunk 2 in
`crates/thegn-svc/src/control/http.rs`, `client.rs`, and the generated control
schema path; chunk 2 must start from this commit. Chunk 3 is downstream but
file-disjoint from this chunk.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core control_error`
- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc control_schema`
- `cargo nextest run -p thegn-svc control_error`
- `git diff --check`

Do not run a live daemon, e2e, `just test`, `just ci`, or a full-workspace
compile. If a manual `thegn` command is unavoidable, prefix it with a fresh
temporary `XDG_STATE_HOME` and do not reuse the worktree's state.

## Done criteria

- Every covered HTTP error body has both `error` and a stable `code`; prose and
  status compatibility are tested.
- `ControlRequestError::code()` is optional and old error bodies still parse.
- No auth/scope/interlock behavior changed; gRPC/MCP/plugin wire contracts stay
  transport-native.
- `docs/api/control-v1.json` matches `control_schema` exactly.
- The scoped tests above and all relevant existing control tests pass.
- Commit exactly as: `fix(the-34): add stable control error codes`
