# Document the dev-loop heavy-gate policy

Linear: THE-4

## Why

The repository already has a documented and mechanically enforced policy for
keeping iterative work off expensive full-workspace checks. `CLAUDE.md` names
the scoped loop, the pre-push/CI tiers, and the `PreToolUse` guard in
`test/heavy-guard.sh`. The remaining issue is inconsistent guidance in the
human contributor docs, the bundled TUI/pipeline skills, and in-app help:
some recipes still make a full build or e2e run look like an every-edit step.

The audit also found that the existing `document-dev-loop-policy` OpenSpec
draft is stale. It claims the contributor docs are already consistent and
promises no docs rewrite, while `docs/local-ci.md`, the TUI guidance, and the
coverage table still contradict or under-specify the policy. The guard is
already implemented and wired; this change documents its current behavior and
does not broaden its matcher.

## What Changes

- Align contributor docs, README, local-CI and coverage guidance on
  `just quick <crate>` plus filtered package-scoped nextest during iteration.
- Make the persistent isolated `muse session` the TUI look/act/look loop;
  reserve `just e2e` and `just e2e-update` for intentional final UI work.
- Add the same short guidance to in-app help and the bundled TUI and pipeline
  skills.
- Reconcile the OpenSpec proposal, design, tasks, and guard delta; sync the
  corrected delta into `openspec/specs/architecture-gates/spec.md`; archive
  the completed change under the current date.

## Non-goals

- Changing `test/heavy-guard.sh`, `.claude/settings.json`, the git hooks, or
  the pre-push/CI policy.
- Changing the justfile, flake, Rust code, configuration, ratchets, control
  API snapshots, or runtime behavior.
- Expanding the shell matcher to recognize additional command forms.

## Impact

- Roadmap: this closes the documentation/help portion of AO.491 (Built-in
  help/docs); the roadmap's Wave 3+ entry also groups THE-4 with the
  completions/config/docs work.
- Specs: `architecture-gates` gains one requirement documenting the existing
  AI-harness guard. It is a harness gate, not a replacement for the git
  pre-push correctness gate.
- Code: none. The guard and hook wiring are deliberately unchanged.
- Runtime/API: none. No config key, action, keybind, zone, panel, provider,
  capability, schema, or worker is added.
