# THE639 mutant 3 independent adversarial review — Luna

Review date: 2026-09-15. This review is source/evidence-only; no Cargo, build,
native rerun, helper/CLI launch, or source mutation was performed. The reviewed
checkout is `/tmp/thegn-THE639-native-03-reader-after-owned-child-exit-20260915`
at `8e2d8ef68558842baab8d5dc0dfdf5ee9a4b08fc`, whose only commit over the
immutable `33c0db71a3efd91a5637be47fd2d353fd628056a` is the stated test-only
counterfactual.

## Verdict

**Approve the completed mutant-3 semantic receipt.** The source mutation is
exactly the reader-order hook: `ChildExited` is emitted immediately after the
existing consuming `wait_child` succeeds (`devcontainer_startup.rs:553-568`),
and the test gate blocks at `ReaderStarted` until that event
(`devcontainer_startup_tests.rs:627-635`). No production ownership, wait,
deadline, or cleanup implementation was changed.

The pinned build admission exited 0, and the receipt-pinned test binary,
depfile, source hashes, and native log hashes match their recorded values. The
exact selector ran once. The log exits 101 after 3.19 seconds at the unchanged
`result.unwrap()` on line 646 with `HeldUnknown(Deadline, ...)`; it does not
show a compile error, gate assertion, cleanup panic, or timeout/backstop
failure.

## Ordering and expected failure

The loop explicitly tests `[0, STDERR_LIMIT]` in that order. For the first
zero-stderr case, the child writes stdout to `Stdio::null()` and no stderr;
the child can exit, `wait_child` reaches `ChildExited`, and the gate releases
the reader. The source then requires `result.is_ok()` before the session,
argv, snapshot, `starts`, explicit finish, and `clean` assertions. Since the
observed failure is the later line 646 unwrap, the zero-output positive
prerequisite passed. This is source-confirmed ordering; the one-test output has
no per-iteration line.

For the second case, the shell fixture emits exactly `STDERR_LIMIT` bytes
(2 MiB) to the retained stderr pipe. The reader is stopped before reading,
so the pipe-filling child cannot finish and cannot produce `ChildExited` before
the caller's three-second deadline. The caller receives the expected
`HeldUnknown(Deadline, ...)`. The `Err` branch calls `h.fixture.finish()` at
line 640 before the positive nonzero-stderr guard and the unchanged
`result.unwrap()` at line 646. The intended first semantic failure is that
unwrap. The 2 MiB argv, config snapshot, starts, second finish, and clean
assertions are later and unreached.

## Custody and bounded cleanup

`finish()` first calls `release()`, which releases every registered gate, then
`finish_operation` polls the exact parked child and reader. Once the reader is
released it drains the pipe, the child exits, the owner joins, and the fixture
directory can be disposed. The source never takes an unfinished handle, and
the log's 3.19-second completion with only the intended line-646 panic shows
that explicit finish completed before the test's unwind. A second Drop path
also calls the same idempotent settlement logic during panic unwind.

The Gate's five-second `wait_timeout` is only an assertion backstop. It was not
the event producing this receipt: `ChildExited` releases the zero-stderr gate,
and the 2 MiB gate is explicitly released by `fixture.finish()` after the
three-second caller result. No gate timeout, outer 30-second timeout, deadlock,
double panic, or unbounded wait is accepted as a mutant kill. The receipt's
semantic failure is valid only because cleanup precedes the intentional unwrap.

## Evidence provenance

Receipt: `/tmp/thegn-THE639-mutant3-native-evidence-20260915/receipt.json`
(`d0e5f6bc3c4d2cc6a116eec9df097c37cd40705f2b195736e86de2c8d39c8bad`). Its
source hashes are reproduced in the accompanying JSON. The binary is
`host-tests` with SHA-256
`f5f998d750b0fe67a7fade2b276984d8f5ec8a88aef01d718eae406a584d8958`; its
depfile SHA-256 is
`ea03e6765bd21b957fd7f6cb693062b23d7080ed24c17eeb301f3f6540c411b5`.
The build admission JSON exits 0 and has SHA-256
`72a5945338c7fee7c7d8ac5e77cac170c1498f96d19d340d61b408bfb5733b4d`; its
JSONL is SHA-256
`7f50b3f693af1a3e58e269c061cbd96621cd75ca5a6ea847649631c25f4d33b1`.
The native log is SHA-256
`24a8ec2b18e4bc0bef02a00e45c8f27af1cfb67fe7b55e0b0114e0b60e3687d2`.
The recorded CPU scope readback is `cpu.max=100000 100000`; this review makes
no stronger scheduling claim.

This approves the mutant-3 counterfactual evidence and its expected semantic
failure. It does not alter the shipping THE639 implementation or claim the
positive full-suite receipt beyond the supplied evidence.
