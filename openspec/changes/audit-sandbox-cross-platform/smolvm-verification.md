# smolvm verification record (task 5)

## Status: NOT verified — `Backend::Smol` stays reserved, no code landed

The smolvm activation is honesty-gated (design §6, the
`mark-unverified-backends` criterion): **no enter/teardown/liveness code and no
matrix-class flip may land before the CLI surface is verified against a real,
live smolvm install** on at least Linux + macOS. That verification requires a
machine with smolvm (libkrun/libkrunfw over KVM/HVF) installed and bootable,
which is not available in this implementation environment.

Accordingly, this change leaves `Backend::Smol` exactly as it was:

- `Backend::verified()` still returns `false` for `Smol` (unchanged) — so the
  support report and the enforcement matrix both carry the `(unverified)`
  caveat, and `liveness_argv(Smol)` stays `None` (a PATH probe only, never a
  guessed verb — the rule that "guessing verbs regressed the Apple backend three
  times").
- The matrix row for `Smol` **under-promises** at `shared-kernel` (the OCI-family
  default), not `guest-kernel`. This is deliberately the conservative direction:
  smolvm _is_ a microVM, so `shared-kernel` under-claims until proven — and the
  matrix test `unverified_backends_under_promise` pins exactly this.
- `Smol` remains absent from `default_backend_chain`, so nothing reaches it by
  accident; it is only ever something a user names explicitly.

## What verification must establish before activation (do NOT skip)

Recorded here so a follow-up on a real install has the checklist:

1. **CLI verbs** — the create / exec / stop / remove equivalents and their exact
   flag spellings (`smolmachines` is the probed binary today; confirm it), plus
   the exit-code and stderr shapes thegn must classify.
2. **Path-preserving directory volume** — `--volume <wt>:<wt>` (or its real
   equivalent) must bind the worktree at its **real absolute path**. This
   invariant is load-bearing and non-negotiable: git worktree metadata carries
   host paths, so a container that cannot honor the real path breaks the sandbox
   contract. If the bind is not path-preserving (single-file mounts are already
   documented as unsupported), the backend **stays reserved** — record why and
   stop (task 5.3).
3. **Only then** (task 5.2): wire `liveness_argv`, the enter/teardown argv
   through the OCI-family arms (or a dedicated `Smol` family if the verbs diverge
   from the docker-clone assumption), flip `verified()` to true, and let the
   matrix row become `guest-kernel` with the caveat dropped.
4. Windows (WHP) rides the same row later; it would be the first _isolating_
   Windows backend and slots into the chain before `jobobject`.

## Candidate-runtime evaluation (design §5) — no other backend to add

- **youki** — same `shared-kernel` class as runc; already reachable via
  `[sandbox] oci_runtime = "youki"` and correctly classified by the fall-through.
  No tier, nothing to build.
- **kata-containers** — containerd-shim-v2 shaped; not reachable from thegn's
  podman/docker `--runtime <binary>` seam. Same `guest-kernel` class is already
  reachable via `krun` on Linux. Left unlisted until an engine seam exists.
- **microsandbox** — a server+SDK over the same libkrun the `krun` runtime tier
  already reaches directly. Ruled out as a backend.
- **agentbox** — an orchestrator product, not a runtime. Nothing to adopt.
- **smolvm** — the one genuine gap-filler (a microVM tier for macOS/Windows where
  `krun`'s KVM-only path cannot go). Phased, verify-then-activate, as above.
