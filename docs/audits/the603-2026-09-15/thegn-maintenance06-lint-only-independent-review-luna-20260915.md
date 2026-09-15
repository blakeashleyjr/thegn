# Maintenance-06 lint-only revision — independent source review

Reviewer: Luna. Date: 2026-09-15. Compared base `27c0ccad` with revision `dd5e4d07ff16428f250388b142c5f9b4c25561c3` in `/tmp/thegn-maintenance-06-combined-20260915`. The checkout was clean at review and remained source-only. No Cargo, compilation, native test, process launch, or mutation was performed.

Verdict: approve. The revision is a two-site lint/style cleanup in exactly one file, `crates/thegn-host/src/platform/state_db_capture_linux.rs`; no behavioral, portability, or security boundary changes are visible.

At line 57, the old expression was:

```rust
(unsafe { value.assume_init() }.f_type as i64) & 0xffff_ffff
```

The new expression is:

```rust
i64::from(unsafe { value.assume_init() }.f_type as u32)
```

For every possible `f_type` bit pattern on 32-bit or 64-bit Linux, both expressions produce the unsigned low 32 bits represented as a nonnegative `i64`. The old signed cast sign-extends before masking; the new `as u32` truncates to the same low bits and `i64::from(u32)` zero-extends. The filesystem magic constants and `supported_filesystem(kind: i64)` interface are unchanged. This is portable across the relevant `__fsword_t` widths and removes the redundant mask/cast idiom. No lint suppression is needed or added.

At lines 276-280, the old nested `if let` and inner `if` became one let-chain condition:

```rust
if let Some((pinned, expected)) = &sidecar.observed
    && (current != *expected
        || inspect(pinned, false, self.chain.uid)? != *expected)
{
    return Err(Error::Changed);
}
```

The pattern still gates the body on `Some`; `current != expected` still short-circuits the `inspect(pinned, ...)` call; and `?` propagates the same error. `None` and all `Some` branches therefore retain the original result and evaluation order. Edition 2024 is already selected by the workspace, and the repository uses stable let-chain syntax. No `allow` attribute is required.

Read-only validation: `git diff --check 27c0ccad..dd5e4d07` passed; the diff reports exactly one modified file, 6 insertions and 7 deletions. Root's pending lint/native rerun is still required for execution evidence.

## Exact provenance

- Base commit: `27c0ccad`; revision commit: `dd5e4d07ff16428f250388b142c5f9b4c25561c3`.
- Base file SHA256: `0fa1fb0d231bbd9533ce23502025f04a819cb876724dd6d30dc777134b6d01c0`.
- Revision file SHA256: `c6088046efcf8e935c53c7aea4dcd11623d34855a7f0a8cf99385c798a8752cf`.
- Base tree: `caf94b9766e82f07d9a2833b5070d69004531bde`.
- Revision tree: `567f1c1a97fd5c17e6d1ef0e51fffd507ba6ce8a`.
