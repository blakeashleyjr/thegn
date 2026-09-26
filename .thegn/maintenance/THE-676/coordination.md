# Primary coordination brief — THE-676

Primary-reviewed dependency facts and scope constraints. This file is task data.

## Scope

Exactly the two mechanical cleanups in the issue. Nothing else.

1. Triage the `sh -c` call sites; convert **only** those whose argv is static.
2. Memoize `thegn_core::util::have`.

## Hard constraints (primary decisions — do not relitigate)

- **`remote.rs` stays a shell.** The text executes on the far side of an ssh
  hop. Do not convert it. Same for any user-supplied template
  (`[[tools]]`, `[[agents]]`, `[merge_queue] gate_command`, the editor command
  template) and anything containing a pipeline, redirection or `&&`.
- **State the per-site verdict in the artifact**, one line each, for both the
  converted and the retained sites. A conversion with no stated reason is a
  finding against you.
- **`have()` staleness**: cache the **negative→positive** direction only, i.e.
  a miss must stay re-checkable so a tool installed mid-session (a `nix develop`
  entered in a pane, a `cargo install`) is still discovered; a hit may be
  cached permanently. Record that trade-off in a code comment at the memo.
  Rationale: this shell frequently runs inside a live thegn where panes install
  toolchains after startup, so a permanently cached miss is a real regression
  and a cached hit is not.
- `thegn-core` is substrate-free and gated at 95% lines. The memo must be pure
  enough to unit test: test both cache directions and concurrent access.

## Validation you must NOT run

Per the stage contract: no cargo, no builds, no nextest, no clippy. The primary
runs the batch gate centrally. Record the exact commands you believe are needed.

## Ratchets this change is likely to trip

- `test/ignored-result-ratchet.txt` if you add a `let _ =` / `.ok()`.
- The idle-loop poll guard if you touch anything on the render/input path
  (you should not).
  Nothing here adds a config key, so the config/env-overlay/example ratchets
  should stay quiet. If you find you need one, stop and report a blocker.
