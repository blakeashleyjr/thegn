# THE-23 — lint cleanup (Lead work order)

files:
  - crates/thegn-host/src/handlers/repo_trust.rs

## Why this exists

Two prior coder rows (304, 308) reported the branch could not pass
`just quick thegn-host` and both declined to act because chunk 3's scope
forbids editing `repo_trust.rs`. The supervisor has now reproduced the failure
directly. **This work order explicitly authorizes editing that file**; it is
owned by this row, not by chunk 3.

Reproduced with `cargo clippy -p thegn-host --bins -- -D warnings`:

1. `repo_trust.rs:34` — `TrustedDevcontainer` fields `config`,
   `source_approved` and `status` are **never read**.
2. `repo_trust.rs:150` — needless borrow: `&dc` → `dc`.
3. `repo_trust.rs:171` — needless borrow:
   `devcontainer::recognized_unapplied(&dc)` → `(dc)`.

## Done criteria

- `cargo clippy -p thegn-host --bins -- -D warnings` passes.
- (2) and (3) are mechanical — apply clippy's own suggestion.
- (1) is NOT mechanical and must not be silenced with `#[allow(dead_code)]`.
  Three unread fields on a trust struct means the devcontainer trust path
  computes state nobody consumes. Decide, and justify in your completion
  artifact, which is true:
  - the fields are genuinely needed by a consumer that was never wired up —
    then wire it, or
  - they are vestigial — then delete them and any code that only exists to
    populate them.
  Prefer deleting over `#[allow]`. If you believe an `#[allow]` is genuinely
  correct, it needs a written justification naming the future consumer.
- Do not change behaviour beyond what the above requires; no refactors.
- Keep every ratchet green (`test/ignored-result-ratchet.txt` especially).
