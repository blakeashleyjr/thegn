# Design

This change is a **policy record**. It introduces no code, no UI, and no state;
it affirms shipped behavior as enforceable requirements.

## Rendering & event loop

none — no rendering or event-loop changes.

## Persistence

none — no schema, DB, or on-disk state changes.

## Invariants

This change records and affirms superzej's opsec posture. It MUST NOT weaken the
sandbox-by-default contract; it only writes that contract down as testable SHALL /
SHALL NOT behaviors.

Posture being affirmed (and the existing surfaces it rests on):

- **Sandbox-by-default for agents (AJ 443).** Each worktree's interactive process
  runs in a container (`podman` → `docker` → `bwrap` → `none`), with the worktree
  bind-mounted at its real path so host-side git reads keep working —
  `crates/superzej-core/src/sandbox.rs`. An agent worktree is containerized unless
  the operator passes the explicit, logged `--no-sandbox` escape (item 362). The
  sealed **Bouncer** agent and the **LLM-proxy** chokepoint are the strongest
  expression of this: agent tool calls route over a unix socket with the network
  sealed, never gaining host exec.

- **No telemetry / local-only default (AJ 441).** State is local SQLite under
  `$XDG_STATE_HOME`; the binary does not transmit usage, code, or prompts off the
  machine. The only outbound traffic is the user's own configured endpoints
  (git remotes, GitHub via `gh`/octocrab, the LLM proxy the user points at).

- **Viewer / VCS client scope.** superzej is terminal-native and delegates editing
  to `$EDITOR` (group AG). It embeds no editor (no Monaco), no browser, and no
  desktop-automation / computer-use surface. These were the audit's explicit
  non-goals and are the inverse of host-exec agent runtimes.

These requirements are the regression gate for the posture: a future change that
grants an agent host execution without an explicit opt-out, adds a telemetry
beacon, or embeds an editor/browser/computer-use surface is a spec violation.
