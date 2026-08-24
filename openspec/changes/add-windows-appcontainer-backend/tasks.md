# Tasks — Windows AppContainer backend

## 1. Prove the assumption before building on it

- [x] 1.1 `examples/appcontainer_conpty_spike.rs`: a contained grandchild both
      writes a ConPTY owned by thegn and reads a keystroke from it, while being
      denied a file its uncontained sibling reads. Two independent controls, both
      load-bearing — the spike reached the wrong verdict twice before this.

## 2. Honesty first

- [x] 2.1 `IsolationClass::OsAccessControl`, below `SharedKernel`, with its own
      escape note.
- [x] 2.2 `WinAppContainer` maps to it; `WinJobObject` moves to `HostProcess`
      (lifetime and resource bounds are not a security boundary).
- [x] 2.3 `IsolationClass::ALL` + a test that every class has a non-empty,
      single-line, kebab-case, unique name and escape note. The old test listed
      the classes by hand, which is how a new variant goes unchecked.
- [x] 2.4 `sandbox_truth::observed` reads the trampoline positionally, so the
      backend is verified rather than trusted; `reconcile`'s win-native exemption
      now covers only `jobobject`. Negative test: merely naming the trampoline in
      an argument must not promote a host shell to "contained".

## 3. The backend

- [x] 3.1 `thegn-core/src/sandbox_appcontainer.rs` — pure planning: profile name
      (hash-truncated so deep siblings cannot collide), grants, capability SIDs,
      the `icacls` argv and the manual-grant hint. 9 unit tests, all runnable from
      the Linux coverage gate; no Win32 in core.
- [x] 3.2 `thegn appcontainer-exec` (hidden) — the trampoline. Derives/creates the
      profile SID, resolves capability SIDs, `CreateProcessW` with
      `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` and `bInheritHandles`, waits,
      exits with the child's status.
- [x] 3.3 `backend_enter_argv` routes the pane through the trampoline with its
      profile and capabilities, `--`-terminated so clap cannot eat shell flags.
- [x] 3.4 `available_probe` reports it Present on local Windows placements —
      because selecting it now actually contains something.
- [x] 3.5 `appcontainer` added to the default backend chain, below the OCI entries
      (a stronger class) and above `host` (no boundary at all).
- [x] 3.6 Host-side `prepare`: create the profile, apply grants, fail on the
      worktree, warn with the exact `icacls` on a toolchain. Wired into the pane
      spawn path so a failure falls through to the next backend.
- [x] 3.7 `join_argv` quoting tests — an unquoted `C:\Program Files\…` would split
      into two arguments and silently run the wrong program.

## 4. End-to-end

- [x] 4.1 `tests/appcontainer_pane.rs` (Tier 2, `THEGN_APPCONTAINER_E2E=1`): the
      real binary, a real ConPTY, the real trampoline. Asserts the contained shell
      reaches the console, an uncontained control READS a guard file, and the
      contained read is denied.
- [x] 4.2 The control is load-bearing and caught a real defect: the first version
      used one compound `cmd` line, which mis-parsed through CommandBuilder's
      quoting and the trampoline's join, so the guard read always failed and the
      containment assertion passed vacuously.
- [x] 4.3 `thegn doctor` verified on-machine: `appcontainer ready
      os-access-control`, `jobobject unsupported host-process`, and
      `--set sandbox.backend=appcontainer` selects it.

## 5. Docs & spec

- [x] 5.1 This change folder.
- [x] 5.2 `config/config.toml.example` — the chain entry, and the stale
      "OCI declines on Windows" note replaced.
- [x] 5.3 `docs/windows-native-audit.md` — part 5 records the backend, what it costs, and the test that lied.
- [x] 5.4 `openspec validate --all --strict` — 89 passed, 0 failed; this change validates by name.

## 6. Not done here

- [ ] 6.1 Job Object resource limits (`pids_limit`, cpu, memory) layered under the
      AppContainer. The plumbing exists (`spawn_grouped`) but takes a
      `std::process::Command`, not portable-pty's `CommandBuilder`, so it cannot
      reach a pane yet.
- [ ] 6.2 Profile teardown. `DeleteAppContainerProfile` is available; profiles are
      currently left behind, which is harmless (they are reused by name) but
      untidy.
- [ ] 6.3 `read_only_root` / `FileAccess` variants are not yet expressed as ACL
      shapes; every contained pane gets worktree-write + toolchain-read.
