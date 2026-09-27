# Primary review — THE-601 revision 1 (reviewing row 629, commit e8a93702)

You implemented all four decisions, including the two that were new — the
descendant-workspace quarantine and fail-closed Windows containment — and you
withheld any gate claim. Good. The primary compiled it, and there are **two
compile errors plus one architectural gate failure**. The third is the one that
matters, and the two errors are symptoms of it.

## The root problem — `gate_capture.rs` must contain ZERO platform `#[cfg]`

`crates/thegn-host/src/gate_capture.rs` is a new file carrying nine platform
`#[cfg]`s: three `make_nonblocking` variants, two `child_exited` variants, and
more. That is a **hard gate failure**, not a style note:
`test/platform-cfg-host-ratchet.txt` is a **shrink-only** allowlist of files
outside `src/platform/` that carry a platform `#[cfg]`, and `gate_capture.rs` is
not on it. The ratchet refuses additions, so `just lint` and `just test` fail on
this file's existence in its current shape. **Do not add it to the ratchet** — the
allowlist is existing debt to be paid down, and adding a fresh entry needs a
reason that does not exist here, because the seam is already built.

Your own code shows the intended shape: line 142 already calls
`crate::platform::gate_pipe_nonblocking(...)` for Windows, and
`platform/windows.rs:871` defines it. Unix simply did not go through the seam.

So:

1. **Move all three `make_nonblocking` bodies into `src/platform/`** —
   `platform/unix.rs`, `platform/windows.rs`, and the unsupported fallback in
   `platform/mod.rs` — behind **one** signature the call site can use with no
   `#[cfg]` at all. `gate_pipe_nonblocking` is already the name; extend it rather
   than inventing a second one.
2. **Do the same for `child_exited`.** `platform::gate_child_exited` already
   exists for Unix; give Windows its implementation in `platform/windows.rs`
   instead of an inline `#[cfg(windows)]` arm calling `try_wait`.
3. **Sweep the rest of the file** for any remaining platform `#[cfg]`, including
   the one at line 92 and the one in the test module at line 621, and route each
   through `platform::`. The target state is a `gate_capture.rs` with no platform
   `#[cfg]` at all, so it never needs a ratchet entry.

## Compile error 1 — this dissolves once the seam is used

`gate_capture.rs:76: error[E0277]: the trait bound R: AsRawFd is not satisfied`

`spawn_reader<R: Read + Send + 'static>` calls `make_nonblocking(&pipe)`, whose
Unix variant requires `AsRawFd` — a bound `spawn_reader` cannot state without
becoming platform-specific itself. **Do not fix this by adding `AsRawFd` to
`spawn_reader`**: that is correct on Unix and breaks the Windows build, and it
spreads the platform detail further into the file.

Once step 1 is done, `platform::gate_pipe_nonblocking` takes the platform-neutral
argument and `spawn_reader`'s bound stays `Read + Send + 'static`. If the seam
genuinely needs a per-platform bound at the boundary, express it as a single
platform-neutral trait defined **in `src/platform/`** and blanket-implemented
there — one name, one bound, and the `#[cfg]` lives where the ratchet allows it.

## Compile error 2 — a path, fix it directly

`integrate.rs:788: error[E0433]: cannot find module or crate integrate_gate`

`gate_quarantines()` calls `integrate_gate::poisoned_gate_workspaces()`; it needs
`crate::integrate_gate::poisoned_gate_workspaces()`.

## Confirmed — keep all of this

- Bounded independent stdout/stderr tails, incremental reads, explicit pipe
  settlement, typed result.
- Setup and gate deadlines, `0` = disabled, documented, with the disabled case
  covered by a test.
- **Exit classification unmoved**: setup failure, timeout, transport failure and
  reap failure are `GateVerdict::Error`; only a completed gate exit reaches
  `classify_exit`; nothing of that kind enters `bisect_offender`.
- **Descendant-workspace quarantine** rather than releasing the lease, surfaced in
  `doctor`. That was decision 3 and you built it — a quarantined lease must stay
  visible, because a silently non-reused warm worktree turns every later fold into
  a cold compile.
- **Fail-closed Windows containment before spawn.**
- Your honesty about the limits: filesystem and uninterruptible OS I/O remain
  outside any configured deadline, and uncertain ownership keeps the workspace
  quarantined. Keep those statements in the comments.

## Also check while you are in there

`crates/thegn-host/clippy.toml` **bans blocking child waits** outside sanctioned
sites. `child_exited`'s Windows arm calls `child.try_wait()`, which is not a
blocking wait, but confirm the lint's view and use the existing
`#[expect(clippy::disallowed_methods)]`-with-a-reason pattern if it objects.

## Validation

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and
`cargo nextest run -p thegn-host gate_capture`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary will re-run the check, the targeted tests, clippy and
the ratchet regardless.

Never report a verdict for code you could not compile. Withholding it last round
was right; do the same again if you cannot compile.
