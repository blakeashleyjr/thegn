# Design — debugger audit + adapter registry

## Audit method

Read `openspec/specs/debugger/spec.md` requirement-by-requirement against
`thegn-core/src/debug.rs` (pure: tool spec, platform gate, argv builders —
all unit-tested) and `thegn-host/src/cmd/debug.rs` (I/O: resolver install,
exec-replace), plus the doctor's managed-tools section
(`cmd/doctor.rs::managed_tools_report` — reports `bugstalker` with a
platform-gate note) and the roadmap (AQ 518, 525–528). External check:
BugStalker upstream is at 0.4.8 (pin: 0.4.6) and now ships a DAP server
(stdio + TCP). Every spec scenario is implemented; the findings are
omissions and the missing seam, not broken claims.

## Adapter registry model

```toml
[[debug.adapters]]
name = "lldb"
run = ["lldb", "--", "{program}"]      # {program} + trailing debugee args
attach = ["lldb", "-p", "{pid}"]
platforms = ["linux-x86_64", "darwin-aarch64", "darwin-x86_64"]
```

- **Built-in as data**: `bs` is the default entry — `run = ["{bin}",
"{program}"]`, `attach = ["{bin}", "-p", "{pid}"]`, platforms
  `["linux-x86_64"]` — where `{bin}` is the managed-tool resolution
  (override → PATH → managed install at the pin). User entries have no
  managed tier: their argv[0] resolves from PATH or an absolute path, and
  `{bin}` is not available to them.
- **Template substitution is pure** (`thegn-core`): `{program}`, `{pid}`,
  trailing debugee args appended after the template for `run`. Unknown
  placeholders are a config-validation error. All argv construction stays
  unit-tested pure logic, like today's builders.
- **Selection**: `thegn debug run|attach --adapter <name>`; default `bs`.
  Unknown name ⇒ refusal listing known adapters. `thegn debug setup` and
  the managed pin apply to `bs` only; `thegn debug path` reports the
  selected adapter's resolution.
- **Per-adapter platform gate**: the pure predicate takes the entry's
  `platforms` list; the refusal message names the adapter and its supported
  platforms. `debug.rs`'s global gate becomes the `bs` entry's gate.

## Event loop / rendering

None touched. Every debug verb is a CLI subcommand that exec-replaces (or
prints); nothing runs on the compositor loop, no damage channel, no wake
path, no new interactive surface (hence no new help context key — the
existing `docs/help/cli.md` table row covers the verb; the `--adapter` flag
and config table land in the generated config-reference page plus
`configuration.md` prose).

## SQLite

No schema change; no `user_version` bump.

## Alternatives considered

- **Build the DAP client substrate now** (AQ 525–528) and treat every
  debugger as a DAP adapter — rejected here: it is a Tier-2 programme
  (service seam in thegn-svc, breakpoint state, three new panel surfaces).
  The registry is deliberately the thin generalization of what exists — an
  interactive-launcher table — and gives DAP work a config surface later.
  BugStalker's new DAP server makes the future path concrete.
- **Keep single-vendor and only fix the spec prose** — rejected: the audit's
  weightiest finding is exactly that a Python/Go/C++ worktree (or any
  non-Linux-x86-64 host) has no debugger story; "seams, not vendors" is a
  house invariant and the fix is cheap because argv construction is already
  pure and template-shaped.
- **Managed-tool specs for gdb/lldb/delve** — deferred: pinning and
  installing foreign toolchain debuggers is distro territory; PATH
  resolution is honest for user adapters today.
- **A capability-catalog row for debug verbs** — rejected: `thegn debug` is
  a local exec-replace, not an external door; projecting "exec a debugger on
  the host" over HTTP/MCP would be a new remote-execution surface nobody
  asked for.

## Security

- **Adapter argv is subprocess argv from config.** Same trust stance as the
  LSP registry: entries MUST resolve only from trusted config layers;
  worktree-layer `[[debug.adapters]]` entries are ignored with a notice
  until `add-config-trust-resolution` lands. (Lower exposure than LSP —
  adapters run only on an explicit `thegn debug` invocation, never
  automatically on panel open — but the same rule applies on principle.)
- **ptrace blast radius**: `debug attach <pid>` reads and controls the
  target's memory. This is the invoking user's existing privilege (YAMA
  `ptrace_scope` and same-uid rules apply, unchanged by thegn); thegn adds
  no elevation, no setuid, no capability grants. Inside a sandboxed pane the
  exec-replaced debugger is confined by the pane's sandbox — attach can only
  reach PIDs visible inside it, which is the correct boundary and is why the
  exec-replace contract is kept verbatim.
- **No secrets, no SecretRef surface**: entries are argv only; the pane
  environment passes through as for any interactive program.
- **No new write door**: no catalog row, no remote invocation, no daemon
  verb.

## Open questions

- Should the refusal on an unknown adapter suggest `[[debug.adapters]]`
  config with an example, or just list known names? Leaning list + a
  one-line config pointer.
- Per-worktree default adapter (a Rust repo defaults to `bs`, a Go repo to
  `delve`) — deferred; needs a language→adapter mapping that the LSP
  registry's extension table could one day feed.
