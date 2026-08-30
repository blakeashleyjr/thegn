# THE-61 security / test / bug review

PASS

The branch was first merged with `main` in `f8bef0f9`, and the full review
surface was `git diff main...HEAD`. The THE-61 change is documentation and
OpenSpec only: six ADRs, an index, the archived change, and the canonical
architecture-gates requirement. It adds no subprocess, shell, config-file,
permission, DB, event-loop, network, capability, or runtime error path.

One documentation defect was found and fixed in `5ff7a096`: the dependency
gate text incorrectly said every RustSec advisory fails. `deny.toml` makes
vulnerability advisories errors while unmaintained, unsound, and notice
categories retain their configured severity. The canonical spec, archived
delta, and pipeline design now state that distinction consistently.

Validation after the fix:

- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= just quick thegn-core` — passed.
- `cargo nextest run -p thegn-core crate_boundaries` — compiled; no matching
  tests (0 run, 3,645 skipped), as expected from the lane checklist.
- `treefmt --ci` in the pinned Nix shell — passed, 2,275 formatted, 0 changed.
- `openspec validate --all --strict` in the pinned Nix shell — 169 passed, 0
  failed.
- Static ratchets — forge-leak, runtime-leak, async-trait, ignored-result, and
  element clean. `json-emit` reports the pre-existing
  `crates/thegn-host/src/cmd/session_fork.rs` violation from `main`; that file
  is absent from `git diff main...HEAD` and was not changed here.
- `git diff main...HEAD --check` and `git diff --check` — passed.

No e2e tests were run. No `thegn` binary, migration, or live state DB was
invoked. The knowledge graph is absent, so no diff overlay was generated.

## Snapshots

None; no frame-affecting changes.
