# THE-26 chunk 3 — debugging workflow documentation

## Scope

Give operators one truthful entry point for debugging thegn itself and
clarify the current, deliberately narrow support for debugging users’
programs. This is documentation and help registration only.

## Files touched (exact)

- `docs/help/debugging.md` (new)
- `docs/help/index.md`
- `docs/help/cli.md`
- `crates/thegn-host/src/help/pages.rs`
- `config/config.toml.example`

No other files are in scope. Do not add config keys or change runtime logging,
panel behavior, debugger behavior, or ratchet baselines.

## Approach

1. Add a `debugging` help page using the existing frontmatter/page-source
   convention. Cover, with commands and limits:
   - `thegn doctor` and `thegn doctor --json`;
   - `thegn doctor bundle --out ...`, its redacted contents, bounded tails,
     current-process ring, and separate-process limitation;
   - `THEGN_LOG` filtering, the opt-in tracing file sink, and raw stderr
     capture;
   - `THEGN_PERF=1`, `thegn::perf`, the existing tuning env vars, the live
     Telemetry LOOP overlay, and feature-gated SIGUSR2 flame graphs;
   - `thegn debug setup/path/run/attach` as BugStalker-only on Linux x86-64;
     the reserved panel placeholder; and the absence of DAP/gdb/lldb pane
     integration in this release.
   - Include a short warning to review a bundle before sharing it.
2. Link the page from `docs/help/index.md` and add a concise CLI help pointer
   in `docs/help/cli.md`. Register it in `pages.rs` so the generated help
   catalog includes the new source. Do not add frontmatter actions or context
   keys unless an existing page convention requires them.
3. Correct only the `[log]`/diagnostics comments in
   `config/config.toml.example` so tracing stderr, the host file sink, and raw
   stderr capture are not conflated. Do not introduce a new key.

## Overlap and dependency

Independent of chunks 1 and 2; no shared files or ordering dependency. The
Lead may run this chunk in parallel with both other chunks. The page should
describe Chunk 1’s eventual ring entry, but it must remain accurate if read
before that code lands by clearly marking the bundle’s current-process scope.

## Tests to run

From the worktree:

- `just quick thegn-host`
- `cargo nextest run -p thegn-host help`

Do not run `just test`, `just ci`, a full-workspace compile, or e2e. No runtime
invocation is needed for documentation; if one is used for a help smoke check,
set `XDG_STATE_HOME="$(mktemp -d)"` first.

## Done criteria

- The new page is registered, linked, generated, and passes the existing help
  source/page tests.
- The workflow distinguishes current-process bundle data from live daemon data,
  tracing logs from raw stderr capture, and the live-only perf/Telemetry views.
- The page makes no claim of DAP, gdb/lldb pane, breakpoint, stepping,
  variables, or launch-configuration integration.
- No config/env/CLI/control key is added; env-overlay, completion-slot,
  control-schema, help-action, help-context, and help-panel-prose ratchets are
  verified unchanged.
- The exact commit subject is:

  `docs(the-26): document debugging surfaces`
