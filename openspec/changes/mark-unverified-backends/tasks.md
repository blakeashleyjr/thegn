# Tasks

## 1. Name the gap

- [x] 1.1 `Backend::verified()` in `thegn-core/src/sandbox.rs`, documenting the
      criterion (a checked liveness verb), the two backends that fail it, and
      the `06ec12ff` bug this is the same shape as.

## 2. Report it

- [x] 2.1 `BackendSupport.caveat` — separate from `remedy` so it can ride a
      `Ready` row without displacing a state remedy on a stopped one.
- [x] 2.2 `caveat_for(backend, state)`, suppressed for `Unsupported` so an
      OS-decided row is not given a second, unrelated reason.
- [x] 2.3 Render it in `thegn doctor`, after the remedy — the "start it" line is
      the more immediately actionable of the two.
- [x] 2.4 Warn at launch in `agent.rs` when the selected backend is unverified,
      matching `unsupported_hardening`'s rule of reporting the sandbox in force.

## 3. Ratchet

- [x] 3.1 Assert the unverified set is exactly `{smol, wsl}` in both directions —
      the real risk is a _working_ backend silently gaining a caveat that tells
      users to distrust it.
- [x] 3.2 Assert neither is in `default_backend_chain`, since the caveat's claim
      that the user selected it explicitly depends on that.
- [x] 3.3 Assert an unverified row keeps both its remedy and its caveat.

## 4. Gates

- [x] 4.1 `cargo nextest run --workspace` (4936 passed).
- [x] 4.2 `cargo fmt --all` + `clippy --workspace --all-targets -D warnings`.
- [x] 4.3 `just smoke`; `just coverage` (core ≥95%).
- [x] 4.4 Verified by hand: `doctor` with a chain of `smol,wsl,podman,host` shows
      the caveat on `smol` alongside its install remedy, and _not_ on `wsl`,
      which is unsupported on this host.

## 5. Not done, deliberately

- [ ] 5.1 Verify `smol`/`wsl` verbs against real installs and add `liveness_argv`
      arms. Requires a real smolmachines install and a Windows host; guessing is
      the failure mode this change exists to avoid.
