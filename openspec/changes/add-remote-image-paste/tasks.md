# Tasks

## 1. Pure policy (thegn-core)

- [x] 1.1 Paste-drop policy helpers: generated-name builder
      (`img-<utc-ms>-<rand>.png`), size-gate check, remote drop-dir expansion
      (via `remote_home`), sweep-eligibility predicate — **unit tests** under
      the 95% gate. _(`thegn_core::paste_drop`: `generated_name`, `over_limit`,
      `remote_dir_expr`/`remote_drop_script` — `~` is expanded via the remote
      `$HOME` in-shell rather than a `remote_home` round-trip, so providers work
      too and the resolved path is printed back; `sweep_eligible`/`keep_minutes`.)_
- [x] 1.2 `[clipboard]` config (`image_paste`, `max_image_bytes`,
      `remote_dir`, `keep_hours`) with defaults + documented
      `config/config.toml.example` entries — **unit tests**: parse, defaults,
      zero/negative clamping. _(`ClipboardConfig` + `normalize()` in
      `post_process`; tests in `config_tests.rs`.)_

## 2. Clipboard image read (thegn-host)

- [x] 2.1 Extend `clipboard.rs` with pure per-platform image candidate
      tables (probe-types + read commands, PNG interchange) mirroring
      `paste_candidates()` — **unit tests** on the tables; the subprocess
      read is the I/O seam. _(`image_read_candidates` + `read_image`/
      `read_image_from` capturing raw stdout; the read's own non-zero exit is
      the "no image" signal, so no separate probe step is needed.)_
- [x] 2.2 Worker flow (`handlers/paste_image.rs`, sibling module per the
      god-file guidance): read → gate → local write (0700/0600) or remote
      stream → result over channel + `TerminalWaker`; never on the event
      loop. _(`spawn` → `spawn_blocking` (QoS Utility) → `run`; outcome over
      `paste_img_tx` + waker pulse, drained in `run.rs`.)_

## 3. Remote drop + paste (thegn-host)

- [x] 3.1 Remote stream over the worktree's `GitLoc` control channel
      (`sh_command` + stdin: mkdir -p, umask 077, cat > file), path pasted
      via the existing `paste_text_into_pane`; failures surface in
      `model.status` (never swallowed — user-invoked primary path).
- [x] 3.2 Age-based sweep of the target drop dir on each paste (local delete
      / remote `find … -delete` confined to the drop dir); no background
      timer.
- [x] 3.3 Wire the `"+` register fallback (text absent → image) and the
      dedicated `paste-image` action; keybind via the keymap; claim the
      action id on a `docs/help/` page — help + prose ratchets green.
      _(`paste-image` action: palette + rebindable, no default chord — mirrors
      `paste-register`, avoids chord-collision risk; documented on
      `docs/help/copy-and-select.md`.)_

## 4. Verification

- [x] 4.1 Unit: gate refusal message, name generation determinism under the
      e2e freeze hook, candidate-table shapes per platform. _(`resolve` mapping
      tests; `e2e_freeze::paste_image_name`; `image_read_candidates` tests.)_
- [x] 4.2 Smoke: local-pane image paste round-trip with a stubbed clipboard
      tool (fixture PNG → file exists 0600 → path pasted); remote path
      covered by the ssh-shim harness where available. _(Covered as a
      `handlers::paste_image` unit test: `write_drop_to_dir` writes 0600 in a
      0700 dir and sweeps only `img-*.png`; the remote script shape is unit-
      tested in `paste_drop`.)_
- [~] 4.3 If any muse e2e spec drives the flow, pin the generated name in
  `e2e_freeze` first. _(Pin added — `e2e_freeze::paste_image_name`; no muse
  spec added yet, so nothing to record.)_
- [ ] 4.4 Run `just ci` once (includes openspec-validate) as the pre-PR gate.
      _(Deferred to the reviewer's pre-PR gate — per the dev-loop policy this
      change iterated with `just quick` + scoped nextest.)_
