# Primary review — THE-601 revision 3 (reviewing row 657)

**The required rule is implemented and I verified it.** Untruncated diagnostics
keep the historical stdout-then-stderr concatenation and phase framing, truncation
markers appear only after real clipping, and you left the existing assertions
alone — so **both `integrate::tests` failures are gone**. `merge()` is exactly the
right shape. The suite is **176/177**.

One failure left, and it is in your own new test.

## The failure

```
FAIL gate_capture::tests::large_dual_stream_capture_keeps_only_bounded_tails
  gate_capture.rs:669  assert!(status.success())
```

The fixture is
`head -c 250000 /dev/zero | tr '\000' o; head -c 250000 /dev/zero | tr '\000' e >&2`,
so ~250 KB per stream against `TAIL_BYTES = 64 KiB`.

**Two things to resolve, and the first is the one that matters:**

### 1. The reader must keep DRAINING after its tail is full

`assert!(status.success())` failing on a command that should exit 0 points at the
writer being killed: if the reader stops consuming once `ByteTail` is full, the
pipe fills, and `tr` takes `EPIPE`/`SIGPIPE` and exits non-zero. That would make
**any gate producing more than 64 KiB report as failed** — a false accusation
against a good branch, which is the single thing this area must never do.

So: read to EOF always, and let `ByteTail::push` discard what it does not keep.
Bound the **memory**, never the **consumption**. If the reader already drains and
the non-zero status has another cause, say what it is — do not relax the assertion
to make it pass, because `status.success()` is the property that protects the
branch.

Add the regression explicitly: a gate that writes far more than `TAIL_BYTES` to
both streams and exits 0 must be reported as **`Completed` with a successful
status**.

### 2. Reconcile the assertion's bound with `TAIL_BYTES`

The test asserts `log.len() <= 8_100`, but `TAIL_BYTES` is `64 * 1024` per stream,
so a full dual-stream capture is ~131 KB plus the marker — the assertion and the
constant disagree by more than an order of magnitude. Decide which is intended and
make them agree:

- If 64 KiB per stream is right, the assertion should be derived from the constant
  (e.g. `<= 2 * TAIL_BYTES + MARKER_MAX`), never a bare literal — a hard-coded
  bound silently rots the moment the constant moves.
- If the intended retention is ~4 KiB per stream, change `TAIL_BYTES` and say so;
  the marker text ("showing last 65536 bytes") is generated from it and must follow.

Either way **express the bound in terms of the constant**, so the two cannot drift
again.

## Fixed by the primary already — do not redo

- `shell!` takes no trailing comma (`gate_capture.rs:580`).
- Two POSIX-only tests in `integrate_gate_tests.rs` were `#[cfg(unix)]`; that file
  is not on `test/platform-cfg-host-ratchet.txt` either, so they would have failed
  the ratchet exactly as `gate_capture.rs` did. Now a runtime
  `posix_shell_gate_only()` skip on the file's existing
  `sandbox_backend::host_os()` idiom.
- `recv_tail` returned `TailResult` into four `String` positions (E0308). Added
  `recv_log`, which renders a single-stream degraded tail **through `merge`** so the
  truncation marker follows one rule everywhere and an untruncated tail still
  renders byte-identically.

## Confirmed — keep all of it

Zero platform `#[cfg]` in `gate_capture.rs`; independent fixed-capacity tails;
setup and gate deadlines with `0` = disabled and the disabled case tested; **exit
classification unmoved** (setup failure, timeout, transport failure and reap
failure are `GateVerdict::Error`, only a completed gate exit reaches
`classify_exit`, none enter `bisect_offender`); descendant-workspace quarantine
surfaced in `doctor`; fail-closed Windows containment before spawn; and the honest
comments about filesystem and uninterruptible I/O sitting outside any deadline.

## Validation

`nix develop --command cargo nextest run -p thegn-host -E 'test(integrate) +
test(gate) + test(platform_ratchet)'` and `cargo check -p thegn-host
--all-targets`. **The pipeline sandbox mounts `/nix/store` read-only, so this
usually fails outright** — say exactly that and stop if it does. The primary
re-runs all of it regardless.

Never report a verdict for code you could not compile; state what you could not
run.
