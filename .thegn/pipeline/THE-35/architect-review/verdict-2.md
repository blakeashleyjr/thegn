# THE-35 architect review

APPROVED

## Review basis

The branch was reviewed against `main...HEAD`, the THE-35 design and lane
documents, `CLAUDE.md`, and `docs/ARCHITECTURE.md`. `git merge main` was run
first and reported that the branch was already up to date; the existing merge
commit is `8b74a282`.

The revision-1 hydration gap is resolved: mentioned and overdue hydration now
uses the core `record_once` funnel. The sound provider remains at the host
edge, the core policy is substrate-free, event-loop delivery is bounded and
non-blocking, and configuration/help/example/OpenSpec documentation is
consistent.

Two small issues found during this review were fixed and committed:

- `34e0e700` — pinned the test-only platform cfg in the host ratchet.
- `7b1ade37` — consumed both normal and fallback bell latches atomically and
  added strict validation for profile sound maps, with regression coverage.

## Verification

- Core mandatory nextest filter: 326 passed before one unrelated existing
  failure, `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv`, which
  reports `GH_TOKEN=ghp_secret` on OCI argv. The failure is outside THE-35.
- Host mandatory nextest filter: 104/104 passed.
- Service control schema: 1/1 passed.
- `JUST_TEMPDIR=/tmp RUSTC_WRAPPER= just quick`: passed.
- Touched-crate clippy with `-D warnings`: passed.
- Nix `treefmt --ci`: passed, 0 files changed.
- Strict OpenSpec validation: 170/170 passed.
- Touched-crate rustdoc with warnings denied: passed.
- Ignored-result ratchet: clean (318 pinned).
- `test/ratchet-check.sh` is not present.

## Unverified / intentionally out of scope

The lane documents' unverified items were checked where local gates cover
them. Native playback was not exercised on every target OS, and no live
state-DB migration, coverage, full `just test`, `just ci`, or e2e run was
performed per review instructions. The unrelated OCI-secret test remains a
pre-existing repository red and should be handled separately.

No revision chunk is required.
