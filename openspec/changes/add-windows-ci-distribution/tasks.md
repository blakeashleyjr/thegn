# Tasks — native Windows phase 6 (CI promotion + distribution)

## 1. CI

- [ ] 1.1 Promote `windows` from opt-in to routine only after a recorded green
      run: workspace msvc check + ipc + platform kernel tests.
- [x] 1.2 Opt-in Windows runs build and upload `thegn.exe` as the canonical
      `thegn-x86_64-pc-windows-msvc` artifact (30-day retention).
- [x] 1.3 ci.yml header documents the job alongside the macos/e2e notes.
- [x] 1.4 A dependent Windows job downloads the canonical artifact and smokes
      `thegn.exe --version` and `thegn.exe --help` outside the build tree.

## 2. Distribution docs

- [x] 2.1 README "Install" documents the current native-Windows source path
      (rustup + VS Build Tools, `cargo install --path crates/thegn-host`,
      Windows Terminal requirement, Job-Object scoping note) and names the
      configured CI artifact without presenting it as available before a green
      run.
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
- [ ] 4.3 First opt-in `windows` plus `windows-artifact-smoke` CI run green on
      GitHub; then remove the opt-in gates and record the first routine run.
