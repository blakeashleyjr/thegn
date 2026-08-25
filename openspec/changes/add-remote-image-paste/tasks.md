# Tasks

## 1. Pure policy (thegn-core)

- [ ] 1.1 Paste-drop policy helpers: generated-name builder
      (`img-<utc-ms>-<rand>.png`), size-gate check, remote drop-dir expansion
      (via `remote_home`), sweep-eligibility predicate — **unit tests** under
      the 95% gate.
- [ ] 1.2 `[clipboard]` config (`image_paste`, `max_image_bytes`,
      `remote_dir`, `keep_hours`) with defaults + documented
      `config/config.toml.example` entries — **unit tests**: parse, defaults,
      zero/negative clamping.

## 2. Clipboard image read (thegn-host)

- [ ] 2.1 Extend `clipboard.rs` with pure per-platform image candidate
      tables (probe-types + read commands, PNG interchange) mirroring
      `paste_candidates()` — **unit tests** on the tables; the subprocess
      read is the I/O seam.
- [ ] 2.2 Worker flow (`handlers/paste_image.rs`, sibling module per the
      god-file guidance): read → gate → local write (0700/0600) or remote
      stream → result over channel + `TerminalWaker`; never on the event
      loop.

## 3. Remote drop + paste (thegn-host)

- [ ] 3.1 Remote stream over the worktree's `GitLoc` control channel
      (`sh_command` + stdin: mkdir -p, umask 077, cat > file), path pasted
      via the existing `paste_text_into_pane`; failures surface in
      `model.status` (never swallowed — user-invoked primary path).
- [ ] 3.2 Age-based sweep of the target drop dir on each paste (local delete
      / remote `find … -delete` confined to the drop dir); no background
      timer.
- [ ] 3.3 Wire the `"+` register fallback (text absent → image) and the
      dedicated `paste-image` action; keybind via the keymap; claim the
      action id on a `docs/help/` page — help + prose ratchets green.

## 4. Verification

- [ ] 4.1 Unit: gate refusal message, name generation determinism under the
      e2e freeze hook, candidate-table shapes per platform.
- [ ] 4.2 Smoke: local-pane image paste round-trip with a stubbed clipboard
      tool (fixture PNG → file exists 0600 → path pasted); remote path
      covered by the ssh-shim harness where available.
- [ ] 4.3 If any muse e2e spec drives the flow, pin the generated name in
      `e2e_freeze` first.
- [ ] 4.4 Run `just ci` once (includes openspec-validate) as the pre-PR gate.
