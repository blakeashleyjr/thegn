# THE-88 security/test/bug review

PASS

Ready for the merge queue. `main` was merged first and the full `main...HEAD`
diff was reviewed, including all coder `Unverified` sections and the accepted
architect follow-ups.

Findings fixed and committed separately:

- `c1aecc35`: report/note inputs now strip terminal control characters and the
  core DB write API reapplies the caps/policy.
- `ebfba819`: row-less/`--any` waits require a positive hard timeout, enforce it
  around the complete control request, and pane exits cannot auto-complete
  pipeline/artifact rows before report/artifact verification.
- `8916cdf6`: v61 migration output is verified before `user_version` is stamped,
  so swallowed ladder errors cannot make an incomplete upgrade look complete.
- `82f08313`: the bundled monitor explicitly treats report/note text as opaque
  data and forbids shell, prompt-template, or unquoted-argument interpolation.

Verification:

- Mandatory core filter: 522 passed.
- Mandatory host filter: 121 passed.
- Focused core report/dispatch/migration filter: 31 passed.
- Focused host dispatch/PTY filter: 56 passed.
- `cargo clippy -p thegn-host --tests -- -D warnings`: passed.
- `mq_assets` clap-resolution/frontmatter filter: 8 passed.
- `cargo fmt --all -- --check` and `git diff main...HEAD --check`: passed.
- All test/migration checks used temporary `XDG_STATE_HOME`; no live state DB,
  daemon, binary, or e2e run was used.

Unverified: full workspace gates, coverage, smoke/e2e, docs, and cross-platform
checks remain intentionally unrun under the scoped-review policy.

Frame impact: no TUI rendering code or snapshot fixture changed; no frame
snapshots need re-recording. CLI human output and the bundled pipeline skill
changed only.
