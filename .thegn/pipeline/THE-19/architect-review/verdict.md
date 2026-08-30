REVISE

Revision chunk: `.thegn/pipeline/THE-19/architect-review/revision-1.md`

The branch was first merged with `main` in `bf4f8746`, then reviewed as the
full `main...HEAD` diff. I made and committed these small mechanical fixes:

- `3ab576d6 fix(the-19): keep hook process control behind platform seam`
- `4137a74 fix(the-19): satisfy hook policy clippy`

The implementation is not ready to land because normal sidebar/workspace
deletion forces blocking hooks and prunes source-of-truth state before the
background result, session-end is not wired to last-pane/tab boundaries, the
core resolver reads repo files, create rollback bypasses lifecycle hooks, and
the OpenSpec log/notification contract is not synchronized. The revision chunk
has the concrete file/line findings and expected fixes.

Required checks:

- core filtered nextest: PASS (527)
- host filtered nextest: PASS (104)
- `thegn-svc --test control_schema`: PASS (1)
- `just quick`: PASS
- touched-crate clippy with tests and `-D warnings`: PASS
- `treefmt`: PASS, no drift
- `openspec validate --all --strict`: PASS (170)
- rustdoc for touched crates with `-D warnings`: PASS

Unverified or unavailable: full workspace/e2e and smoke execution were not
run, as directed; the lane's literal filters with no matching test names were
covered by their equivalent integration tests; `test/ratchet-check.sh` is not
present; and the PATH `thegn` binary reports that `dispatch` is unsupported,
so no dispatch report could be filed. No live state DB was used.
