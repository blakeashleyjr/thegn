# Add native Windows support, phase 6: CI promotion + distribution

## Summary

The final phase of the native Windows port is process, not behavior:

- **CI promotion**: the `windows` job (windows-latest, bare rustup — no nix)
  moves from opt-in (`[ci-windows]` marker) to **routine** on push/PR: it is
  the msvc truth gate (workspace `cargo check --locked`) plus the
  real-kernel semantics tests (named-pipe IPC round-trip/bind-lock, Job
  Object terminate-/drop-reaps-tree). The per-PR Linux-side
  `cargo check --workspace --target x86_64-pc-windows-gnu` in
  `just check-cross` remains the cheap cfg-regression gate.
- **Distribution**: the windows job builds `--release` and uploads a
  `thegn-x86_64-pc-windows-msvc` artifact (`thegn.exe`) from every run —
  the native-Windows download until a tagged-release pipeline exists. The
  documented install paths are `cargo install --path crates/thegn-host`
  (README "Install → Windows") and the CI artifact; the dev loop is
  CONTRIBUTING "Windows (native) notes".
- **Spec sync**: the five phase changes' `platform-windows` deltas are folded
  into the new main spec `openspec/specs/platform-windows/spec.md`
  (`openspec validate --all --strict` green with the in-flight changes still
  present). The changes stay **unarchived** until the on-machine validation
  checklist (`add-windows-compositor-validation` tasks §2 + the parity
  on-machine items) passes on a real Windows box — archiving before the
  interactive behavior is proven would overstate what's verified.

## Impact

- `.github/workflows/ci.yml` (windows job routine + artifact), README
  (Windows install), `openspec/specs/platform-windows/spec.md` (new
  capability spec), tasks.md AX group notes.
- No shipped-binary behavior change.
