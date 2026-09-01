# THE-55 Architect Review 3

REVISE

Revision chunk: `.thegn/pipeline/THE-55/architect-review/revision-3.md`

The branch was reviewed after the required `git merge main` (already up to
date), using the full `git diff main...HEAD`, the architecture handoff, every
lane-document `Unverified` section, `CLAUDE.md`, and `docs/ARCHITECTURE.md`.

The core/store allowlist, target-first transaction and read-back flow, daemon
ID clearing, profile isolation, default/named source lock guard, CLI-only
capability registration, help/completion surfaces, OpenSpec requirements, and
credential boundary are aligned. I landed the following small corrections in
`f185ae1c` (`fix(the-55): preserve target state and dry-run purity`):

- target-owned pin state no longer falsely blocks a source bundle with no pin;
- source lock probing no longer creates a lock file during dry-run;
- the OpenSpec task wording now matches warning-on-notification-failure
  behavior.

Required gates passed on the final working tree:

- core mandatory nextest selection: 530/530;
- host mandatory nextest selection: 104/104;
- service control schema: 1/1;
- `just quick`;
- touched-crate clippy with `-D warnings`;
- dev-shell `treefmt` (0 files changed);
- strict OpenSpec validation: 170/170;
- touched-crate rustdoc with warnings denied;
- focused migration/lock tests: core 12/12 and host 12/12;
- `test/ratchet-check.sh` is absent.

The remaining gap is the unchecked OpenSpec task 3.4: `test/smoke.sh` has no
hermetic `session move` coverage for cold, `--kill`, collision, or dry-run
paths. The concrete expected fix is recorded in revision 3. Full workspace
`just test`/`just lint`/`just ci`, coverage, smoke, and e2e were not started in
accordance with the lane and review instructions; no command was run against
a live state DB.
