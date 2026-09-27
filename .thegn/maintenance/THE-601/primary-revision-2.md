# Primary review — THE-601 revision 2 (reviewing row 648)

Revision 1 did the structural work: `gate_capture.rs` now carries **zero** platform
`#[cfg]`, so the shrink-only ratchet is satisfied without a new allowlist entry,
and the `integrate.rs` path error is gone. Good — that was the hard part.

The primary then compiled and ran it, and fixed two mechanical things itself
(below). **Two real failures remain, and they are the same defect.**

## Fixed by the primary already — do not redo

1. `shell!` takes no trailing comma; `gate_capture.rs:580` had one (`error: no
rules expected ','`).
2. The two POSIX-only tests in `integrate_gate_tests.rs` were gated with
   `#[cfg(unix)]`. That file is **not** on `test/platform-cfg-host-ratchet.txt`
   either, so those two attributes would have failed the ratchet exactly like
   `gate_capture.rs` did. Replaced with a runtime `posix_shell_gate_only()` helper
   built on the file's existing `sandbox_backend::host_os()` idiom — the tests now
   compile on every platform and skip on Windows, which is strictly better than not
   existing there.

## REQUIRED — bounded capture must be INVISIBLE when it does not truncate

```
FAIL integrate::tests::red_base_is_not_a_candidate_failure_and_keeps_both_gate_phases
  integrate.rs:2618  report.diagnostics.contains("[union] failed\nunion-red")

FAIL integrate::tests::bisect_tests_original_candidate_when_live_ref_moves_during_union_gate
  integrate.rs:2689  report.diagnostics.contains("[prefix 1] failed\noriginal-snapshot-red")
```

Both are existing tests asserting the shape of `report.diagnostics` — the gate
output a fold actually shows an operator. The new independent stdout/stderr tails
changed that text for gates whose output is **far** under any cap, which means the
bounding is altering output it was never meant to touch.

The rule to implement:

> When nothing is truncated, `report.diagnostics` is byte-identical to what it was
> before this change. A cap changes the output only when it actually clips.

Concretely: keep the existing phase framing (`[union] failed\n<output>`,
`[prefix N] failed\n<output>`) and the existing stdout/stderr interleaving or
concatenation order for the untruncated case. Bound the _retention_, not the
_format_. When a tail **is** clipped, say so explicitly in the diagnostics (a
marked truncation line is the right place for the new information) rather than
changing the framing for everyone.

**Do not update these two tests to match new output.** They encode the contract
that a fold's diagnostics are readable and stable; changing them would be changing
the thing under test. If after implementing the rule you believe a format change is
genuinely unavoidable, stop and report why rather than editing the assertions.

## Confirmed — keep all of it

- Zero platform `#[cfg]` in `gate_capture.rs`; per-OS behaviour behind
  `platform::gate_pipe_nonblocking` / `platform::gate_child_exited`.
- Independent fixed-capacity stdout/stderr tails, incremental reads, explicit pipe
  settlement, typed result.
- Setup and gate deadlines, `0` = disabled, documented, with the disabled case
  tested.
- **Exit classification unmoved**: setup failure, timeout, transport failure and
  reap failure are `GateVerdict::Error`; only a completed gate exit reaches
  `classify_exit`; none of those enter `bisect_offender`.
- Descendant-workspace quarantine instead of releasing the lease, surfaced in
  `doctor`.
- Fail-closed Windows containment before spawn.
- The honest limits in the comments: filesystem and uninterruptible OS I/O sit
  outside any configured deadline, and uncertain ownership keeps the workspace
  quarantined.

## Validation

`nix develop --command cargo nextest run -p thegn-host -E 'test(integrate) +
test(gate)'` — **include `test(integrate)`**, which is where both failures are, and
`cargo check -p thegn-host --all-targets`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary re-runs the gate, integrate, ratchet and config suites
regardless.

Never report a verdict for code you could not compile; state what you could not
run.
