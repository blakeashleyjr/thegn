# Tasks — fold-actor land files into the Merged folder

## 1. Pure decision (thegn-core)

- [ ] 1.1 Add `LifecycleEvent::LandedInPlace` to
      `crates/thegn-core/src/merge_lifecycle.rs`; `decide` maps it per the
      design table (`off` → `Unfile`; `move`/`expire`/`detach`/`remove` →
      `FileInto(merged_folder)`; blank `merged_folder` → the existing
      empty-name `Noop` guard applies).
- [ ] 1.2 Extend the exhaustive unit tests: every `on_landed` arm for
      `LandedInPlace`, the master-toggle-off case, and an assertion that
      `LandedInPlace` can never yield `RemoveWorktree` (the leave-in-place
      contract as a table property). Keeps the 95% core line gate green.
- [ ] 1.3 Update `default_config_enables_full_lifecycle` to also pin the
      `LandedInPlace` default (`expire` ⇒ file into "Merged").

## 2. The land call-site (thegn-host)

- [ ] 2.1 In `crates/thegn-host/src/cmd/land.rs`, replace the `Dequeued`
      emission in the `Landed` and `UpToDate` arms with `LandedInPlace`;
      rewrite the comment block to state the new contract (file, never
      remove; `off` still un-files).
- [ ] 2.2 Host-side test (in `merge_lifecycle.rs` tests or a `land` test):
      `apply(..., LandedInPlace)` with the default config files the worktree
      into "Merged" and leaves the directory and branch intact; with
      `on_landed = "off"` it un-files from "Merging" and leaves a user folder
      alone (reuses the existing guard fixtures).

## 3. Docs

- [ ] 3.1 `docs/help/merge-queue.md`: state that a `thegn land` files the
      worktree into the Merged folder (leave-in-place; never removed), and
      that only queue-landed rows are expiry-swept. (Prose only — no new
      action ids, so the help ratchets are unaffected.)
- [ ] 3.2 `config/config.toml.example`: extend the `on_landed` /
      `merged_folder` comments with the land-in-place behaviour (no new key).
- [ ] 3.3 CHANGELOG entry — behaviour change: `thegn land` now files into
      "Merged" instead of un-filing to the repo root.

## 4. Validation

- [ ] 4.1 Confirm no e2e baseline is affected (`just e2e` — the change is
      CLI-side with no chrome delta; re-record only if a case drives a land).
- [ ] 4.2 Run `just ci` once (includes openspec validate + core coverage).
