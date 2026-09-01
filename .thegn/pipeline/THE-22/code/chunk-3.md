# THE-22 chunk 3 — contract, configuration, and help ratchets

## Dependency and ownership

Run after THE-27, chunk 1, and chunk 2. This chunk is file-disjoint from the
implementation chunks and serially depends on their final names and behavior.
It owns the draft openspec alignment so the checked-in contract no longer
describes the rejected per-PR blocker/fingerprint or reply-never-resolve
design.

## Files touched

- `config/config.toml.example`
- `docs/help/pr-queue.md`
- `openspec/changes/add-watched-pr-comment-tasks/proposal.md`
- `openspec/changes/add-watched-pr-comment-tasks/design.md`
- `openspec/changes/add-watched-pr-comment-tasks/tasks.md`
- `openspec/changes/add-watched-pr-comment-tasks/specs/pr-queue/spec.md`
- `openspec/changes/add-watched-pr-comment-tasks/specs/state-db/spec.md`
- `test/help-ratchet.txt`
- `test/help-prose-ratchet.txt`
- `test/help-panel-prose-ratchet.txt`

Do not touch Rust source, `test/env-overlay-ratchet.txt`, completion snapshots,
control-schema snapshots, or capability catalog files: no new config key or
external capability is introduced.

## Approach

1. Update the PR-queue example to document that `watch = ["review"]` on an
   explicit queue row creates one task per unresolved thread, that the current
   configured role/prompt are used, that new comments revise the same task,
   and that automatic reply/resolve requires a verified agent push and a
   provider capability. Document reserved/human fallback and the existing
   cadence; do not add `review_trigger`, an auto-watch default, or a second
   timer.
2. Add the panel/palette `handle` action and per-thread task lifecycle to the
   PR-queue help page. State that all PRs are not watched by default and that
   unsupported/rate-limited resolution remains unresolved for a human.
3. Replace the openspec draft requirements with the final contract: explicit
   watch rows, THE-27 snapshot dependency, pure core derivation, durable
   thread-key dedupe/revision, roster prompt/role, off-loop cadence, event
   `pr.thread_unresolved`, forge seam `resolve_review_thread`, verified-push
   resolution, notifications audit, and no external control/catalog surface.
   Mark already-satisfied THE-27 substrate claims as satisfied rather than
   repeating implementation work.
4. Update only the help ratchet claims required by the new action/prose. Keep
   the env-overlay, completion-slot, control-schema, and capability snapshots
   unchanged because this design adds no config key or public capability.

## Tests to run

Use only scoped checks:

- `just quick thegn-host`
- `cargo nextest run -p thegn-host help`
- `cargo nextest run -p thegn-core config`

If the repository's openspec checker is available, run its targeted validation
for `openspec/changes/add-watched-pr-comment-tasks/`; do not run a full
workspace build. Do not invoke the built binary, migrate the live state DB, or
run e2e/`just test`/`just ci`.

## Done criteria

- Every new or changed config/help claim is present in the example/help source
  and its applicable ratchet in the same commit.
- The example contains no undocumented config key, and the env-overlay ratchet
  remains accurate.
- Openspec specs describe per-thread roster identity and resolve lifecycle and
  no longer require the rejected per-PR `UnresolvedComments` blocker,
  `review_trigger`, per-PR fingerprint, or reply-never-resolve behavior.
- Help accurately communicates explicit opt-in, handle action, bounded prompt,
  and human fallback.
- No public catalog/control/completion snapshot was silently changed.
- Commit exactly as:

  `docs(the-22): align watched-review task contracts`
