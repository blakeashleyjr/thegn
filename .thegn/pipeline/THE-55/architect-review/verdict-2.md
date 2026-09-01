# THE-55 Architect Review 2

REVISE

Revision chunk: `.thegn/pipeline/THE-55/architect-review/revision-2.md`

The full branch diff was reviewed against `main`, the architecture handoff,
all lane documents (including every `Unverified` section), `CLAUDE.md`, and
`docs/ARCHITECTURE.md`. `git merge main` was required and was already up to
date, so no merge commit was needed.

The core/store policy, explicit row allowlist, target-first/read-back flow,
daemon-ID clearing, profile path isolation, CLI-only capability registration,
OpenSpec synchronization, and documentation are otherwise aligned. No code
correction was safe to make locally: the remaining issue changes a deliberate
profile-lock invariant and needs an implementation decision plus tests.

Required revision:

- The source compositor guard is ineffective for the default profile.
  `session_move.rs` calls `profile::instance_running()`, but the default
  profile intentionally does not retain the singleton lock, so an interactive
  default-profile compositor can race source cleanup. Make the guard reliable
  for default and named profiles without introducing competing lock semantics;
  see revision-2.md for the concrete file/line finding and required test.

Verification:

- Core mandatory nextest selection: 328 passed before one unrelated existing
  failure, `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv`.
- Host mandatory nextest selection: 104/104 passed.
- Service control schema: 1/1 passed.
- `just quick`: passed.
- Touched-crate clippy with `-D warnings`: passed.
- Strict OpenSpec validation via the repository fallback: 170/170 passed.
- Touched-crate rustdoc with warnings denied: passed.
- Focused migration suites: core 8/8 and host 11/11 passed.
- `treefmt` could not run because `shfmt` is absent from PATH; no worktree
  formatting drift was produced. `test/ratchet-check.sh` is absent.
- No migration or binary was run against a live state DB.
