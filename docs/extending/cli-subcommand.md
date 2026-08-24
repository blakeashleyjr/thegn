# Add a CLI subcommand

1. Add the variant to the clap `Command` tree in
   `crates/thegn-host/src/main.rs` and a module in `crates/thegn-host/src/cmd/`.
2. Register it in `cli_help::GROUPS` (`crates/thegn-host/src/cli_help.rs`) —
   visible commands must belong to a group.
3. `--json` output goes through `cmd::emit_json` (one compact document,
   stable shape). Exit codes: 0 ok, 1 error, 2 retryable, 3 not found (a typed
   `NotFound` error).
4. Document the grammar in `docs/cli.md` (embedded in the binary and served
   over MCP) and, if user-facing, `docs/help/cli.md`.
5. If the verb drives a running instance, it is a projection of the
   capability catalog — see [capability.md](capability.md).

**Gates:** the `GROUPS` drift test, `test/json-emit-ratchet.txt` (JSON
printed outside `emit_json`), `cli_verbs_cover_catalog`, `test/smoke.sh`.
