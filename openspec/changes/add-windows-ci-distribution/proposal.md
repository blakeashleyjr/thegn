# Add native Windows support, phase 6: CI promotion + distribution

## Summary

The final phase of the native Windows port is process, not behavior:

- **CI promotion**: the `windows` job (windows-latest, bare rustup — no nix)
  is currently opt-in (`workflow_dispatch` with `extras: true`). It is the msvc
  truth gate (workspace `cargo check --locked`) plus the real-kernel semantics
  tests (named-pipe IPC round-trip/bind-lock, Job Object
  terminate-/drop-reaps-tree). A dependent job downloads and smokes the
  canonical artifact. The job must produce a recorded green run before its
  opt-in gate is removed. The Linux-side
  `cargo check --workspace --target x86_64-pc-windows-gnu` in
  `just check-cross` remains the cheap cfg-regression gate.
- **Distribution**: an opt-in Windows run builds `--release` and uploads a
  `thegn-x86_64-pc-windows-msvc` artifact (`thegn.exe`), then a fresh job
  downloads and executes it. This is not yet an advertised release channel.
  The current documented install path is
  `cargo install --path crates/thegn-host` (README "Install → Windows"). The
  CI artifact is named there but explicitly unavailable until its first green
  producer-and-consumer run; the dev loop is CONTRIBUTING "Windows (native)
  notes".
- **Spec sync**: the five phase changes' `platform-windows` deltas are folded
  into the new main spec `openspec/specs/platform-windows/spec.md`
  (`openspec validate --all --strict` green with the in-flight changes still
  present). The changes stay **unarchived** until the on-machine validation
  checklist (`add-windows-compositor-validation` tasks §2 + the parity
  on-machine items) passes on a real Windows box — archiving before the
  interactive behavior is proven would overstate what's verified.

## Impact

- `.github/workflows/ci.yml` (opt-in Windows truth gate + downloaded-artifact
  smoke), README
  (Windows install), `openspec/specs/platform-windows/spec.md` (new
  capability spec), tasks.md AX group notes.
- No shipped-binary behavior change.
