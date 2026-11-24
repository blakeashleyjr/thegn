# Say which sandbox backends are unverified

## Summary

`Backend::Smol` (`smol` / `smolmachines`) and `Backend::Wsl` present a complete
surface with nothing behind it. Both parse, sit in `Backend::ALL_OCI`, answer
`true` from `is_oci()`, and are treated as docker clones for `--user`/`--gpus`.
But `liveness_argv` returns `None` for each, so they fall back to a bare PATH
probe — **"the binary exists" stands in for "the runtime works"**.

That is precisely the defect commit `06ec12ff` fixed for docker and Apple, where
a stopped daemon passed the PATH probe, was selected, and then failed every pane
with nothing to explain it. The comment left in `liveness_argv` is candid about
it: _"`smol`/`wsl` are `None` pending someone verifying their verbs against the
real runtimes — guessing here would regress a backend that currently works."_

Neither is in `default_backend_chain`, so nothing reaches them by accident. But a
user who names one gets `ready` in `thegn doctor` and no signal at all, and a
failure downstream reads as thegn's bug rather than an unfinished backend.

## Approach

**Say so; do not guess the verbs.** Inventing a liveness command is how the Apple
backend ended up emitting `container pull` and `container image exists`, neither
of which exists — three launch-breaking bugs from one round of plausible
guessing. The honest move is to report the gap until someone verifies it against
a real install.

- `Backend::verified()` — a single predicate naming the two, documenting the
  criterion and pointing at the commit whose bug this is.
- `BackendSupport.caveat` — a note that, unlike `remedy`, can accompany a
  **`Ready`** row. Kept separate from `remedy` because "we never checked this
  runtime's verbs" is orthogonal to installed/running: an unverified backend can
  be perfectly installed and still fail every pane, and folding the two would
  either hide the caveat on a `Ready` row or lose the remedy on a stopped one.
- Suppressed for `Unsupported`, which is the stronger and unrelated statement —
  `wsl` on a Mac cannot run however it is configured, so a verification note
  there buries the reason that actually decides the row.
- A warning at launch when one is selected, following the same rule
  `unsupported_hardening` already does: report the sandbox actually in force, not
  the one the config implies.

Selection is deliberately **unchanged**. An explicitly named backend is still
honoured — thegn says what it does not know rather than overriding the user.

## Impact

- `tasks.md` group AX (macOS parity) — carried over as an honesty gap from the
  sandbox audit.
- Affected specs: `sandbox` (backend reporting).
- Affected code: `thegn-core/src/sandbox.rs`, `sandbox_support.rs`,
  `thegn-host/src/cmd/doctor.rs`, `thegn-host/src/agent.rs`.
- No behavior change for any verified backend: a ratchet test asserts the
  unverified set is exactly `{smol, wsl}`, so a working backend cannot silently
  acquire a caveat telling users to distrust it.
