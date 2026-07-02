# Tasks

## 1. Contract parse + mapping (superzej-core)

- [ ] 1.1 Pure parser for the `<command>/<explanation>/<warning>` contract →
      `{ command, explanation, warning? }` and a mapper onto the typed `GitOp` —
      **unit tests**: well-formed parse, missing optional warning, unmapped command
      → "unrecognized" (not executed), malformed XML errors cleanly.
- [ ] 1.2 Pre-execution validation against repo state (rebase refused on base
      branch, squash needs ≥2 commits, force-push flagged) — **unit tests**: each
      guard fires on the unsafe state and passes on the safe state.

## 2. Flow + overlay (superzej-host)

- [ ] 2.1 Host-side model call through the proxy: prose + repo context → contract;
      parse + validate. Add an explain-only mode that returns command + explanation + safety and never executes.
- [ ] 2.2 Confirm/warn overlay (bouncer `ApprovalKind` overlay pattern) showing
      description/command/explanation/warnings with Confirm / Edit / Cancel; on
      confirm run via `gitmut::execute()` off-loop — **render test**: the overlay is
      a chrome overlay repaint, and execution never runs without a confirm.

## 3. Docs + validate

- [ ] 3.1 Document the NL-git assist, the explain-only mode, and the confirm gate
      in the git/agent doc section + `config/config.toml.example`.
- [ ] 3.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
