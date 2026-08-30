# THE-19 revision 1 completion

Implemented every concrete runtime finding in the architect-review verdict.

## Delivered

- Made user worktree deletion transactional: `pre_destroy` runs before
  physical removal, blocking failures keep the worktree visible, and DB/model
  pruning occurs only after confirmed disk success. Force and unattended modes
  use the shared lifecycle cleanup path.
- Made workspace deletion transactional per worktree: successful paths are
  pruned, failed paths remain visible and are all reported, and keep-files
  cleanup remains hook-free.
- Wired `session_end` to last-pane exit and tab-close lifecycle boundaries as a
  warn-only, non-blocking hook; daemon attach/detach is not treated as a
  session boundary.
- Kept hook policy resolution pure in `thegn-core`; repository overlay loading
  and trust-aware resolution stay at the host/config boundary, with unit
  coverage for the filesystem-free resolver.
- Routed both `wt create` rollback paths through shared force lifecycle cleanup.
- Added indexed per-worktree event logs under
  `$XDG_STATE_HOME/thegn/hooks/<slug>/<event>-<n>.log`, bounded/redacted failure
  tails, and failure notifications carrying the tail.
- Added the doctor lifecycle-hook source/trust report and synchronized the
  OpenSpec task contract, including explicit remaining smoke and CI gate
  obligations.

## Commits

- `9830bf0e` — keep hook resolver substrate-free
- `00af0c46` — index and redact hook logs
- `632728a9` — await user destroy hooks before pruning
- `ce323428` — route create rollback through lifecycle cleanup
- `adf06f9b` — keep workspace paths transactional
- `7c794701` — sync lifecycle contract and doctor report
- `a78cf4e3` — satisfy targeted lint after contract sync

## Verification

- `cargo check -p thegn-host --tests` — passed
- `cargo nextest run -p thegn-core hooks` — 8 passed
- `cargo nextest run -p thegn-host hook_run` — 4 passed
- `cargo nextest run -p thegn-host worktree_lifecycle` — 2 passed
- `cargo clippy -p thegn-host --tests -- -D warnings` — passed
- `cargo clippy -p thegn-core --tests -- -D warnings` — passed
- `just quick thegn-host` — passed
- Commit pre-checks reported `treefmt` passed for each commit.

## Disputed

None.

## Unverified

- `openspec validate --all --strict` could not run because `openspec` is not
  installed in this environment.
- Direct `treefmt` could not initialize its `shfmt` formatter because `shfmt`
  is not on PATH; the repository pre-commit formatter passed on the commits.
- The requested smoke coverage and full `just ci` gate were not run by policy
  for this targeted revision pass. E2E tests were not run.
