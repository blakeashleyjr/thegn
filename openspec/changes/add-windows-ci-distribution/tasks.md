# Tasks — native Windows phase 6 (CI promotion + distribution)

## 1. CI

- [x] 1.1 `windows` job promoted from opt-in to routine (push/PR/dispatch):
      workspace msvc check + ipc + platform kernel tests.
- [x] 1.2 Release `thegn.exe` built and uploaded as the
      `thegn-x86_64-pc-windows-msvc` artifact (30-day retention) on every run.
- [x] 1.3 ci.yml header documents the job alongside the macos/e2e notes.

## 2. Distribution docs

- [x] 2.1 README "Install" gains the native-Windows path (rustup + VS Build
      Tools, `cargo install --path crates/thegn-host`, CI artifact, Windows
      Terminal requirement, Job-Object scoping note).
- [x] 2.2 CONTRIBUTING "Windows (native) notes" (landed in phase 4) is the
      referenced dev loop.

## 3. Spec sync

- [x] 3.1 `openspec/specs/platform-windows/spec.md` created from the five
      phase deltas (folded in phase order; MODIFIED requirements applied);
      `openspec validate --all --strict` green (87 items).
- [ ] 3.2 Archive the five `add-windows-*` changes after the on-machine
      checklist passes (`add-windows-compositor-validation` tasks §2 and the
      parity §5.2 items) — deliberately deferred; see proposal.

## 4. Final gates

- [x] 4.1 Workspace clippy clean; fmt clean; unit tests green for every
      touched crate; workspace windows-gnu cross-check green + warning-free.
- [x] 4.2 Core coverage gate (`just coverage`, 95% lines) green with the new
      core modules (shellinv, fsperm, termcaps/basename additions).
- [ ] 4.3 First routine `windows` CI run green on GitHub (happens when the
      merge queue lands this branch; dispatch manually to run it earlier).
