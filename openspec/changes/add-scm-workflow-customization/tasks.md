# Tasks — SCM workflow customization

Testing ground rules for the whole change (from repo memory, they bite):
every git fixture passes `-c commit.gpgsign=false` (a global `gpgsign=true`
otherwise hangs tests at a pinentry) — **and** the signing feature itself must
additionally be verified with signing ON via a loopback-pinentry throwaway key
in the smoke/e2e tier, or the feature ships tested only in its disabled state.
Run tests with `cargo nextest run -p <crate> <filter>`, never bare
`cargo test`; keep full-workspace gates for the end.

## 1. Config (thegn-core)

- [ ] 1.1 `config_enum!` `LandStrategy` (`merge`/`squash`/`rebase|linear`) and
      `StructuralDiff` (`off`/`auto`/`difft`); round-trip tests extend the
      existing config-enum table.
- [ ] 1.2 New `[merge_queue]` keys: `land_strategy`, `land_message` (template),
      `sign_commits`, `rerere` — added to `MergeQueueConfig`, its `Default`,
      and `MergeQueueOverlay` (exhaustive destructure keeps the compiler
      honest).
- [ ] 1.3 New `[git]` keys: `structural_diff`, `auto_fetch_colocated`
      (default false).
- [ ] 1.4 `GitOverlay` + `[workspace.<slug>.git]` and `Config::repo_git(root)`,
      mirroring `repo_merge_queue`; unit tests for precedence.
- [ ] 1.5 Document every key in `config/config.toml.example`, including the
      `[[tools]] diff`-seed interaction note; `THEGN_*` env overrides via the
      existing `env_overlay` completeness test.

## 2. Land strategy (thegn-core fold + thegn-svc plumbing)

- [ ] 2.1 `fold::fold` takes a strategy; squash = single-parent `commit_tree`;
      rebase = per-commit `merge-tree --merge-base` replay via a new
      `PlumbingOps` method; conflicts under any strategy defer identically.
- [ ] 2.2 `land_message` template rendered by the `agent_task` brace-var
      engine (`{branch}`, `{target}`, `{subjects}`); validated in
      `config_validate` beside the queue prompts.
- [ ] 2.3 Pure unit tests in core (95% gate): strategy selection, ancestor
      no-op per strategy, replay-stops-on-conflict; svc plumbing covered by
      fixture repos in svc tests.

## 3. Signing (thegn-svc / thegn-host)

- [ ] 3.1 `snapshot_worktree` gains `gpg_args(override_gpg)` (bug fix,
      independent of the new keys).
- [ ] 3.2 `commit_tree` grows a `sign: bool` (⇒ `-S`), null-stdin +
      `GIT_TERMINAL_PROMPT=0`; wire from `[merge_queue] sign_commits`.
- [ ] 3.3 Signing failure classifies as infrastructure error in
      `attempt_land` — drain stops with reason, branch never blamed, agent
      never woken; table-test the classification.
- [ ] 3.4 Loopback-pinentry fixture proving a signed fold produces a `gpgsig`
      header (smoke tier); disabled-signing fixtures stay the default
      everywhere else.

## 4. doctor posture (thegn-host)

- [ ] 4.1 Workflow posture section: git version vs the `merge-tree
--write-tree` floor (≥ 2.38), jj colocation per workspace, declared
      custom merge drivers (`check-attr` sample), signing readiness (only
      probed when `sign_commits` on); `--json` parity.

## 5. Merge drivers + rerere (thegn-host integrate)

- [ ] 5.1 Fixture-audit: does `merge-tree --write-tree` run a custom
      `merge=` driver on supported git versions? Record the answer in this
      change's design and the config docs.
- [ ] 5.2 Conflicted-path `check-attr merge` batch (off-loop); driver-governed
      conflicts route the branch through the throwaway-worktree real-merge
      path (shared with `regenerate_merge`), result tree feeds the fold.
- [ ] 5.3 `[merge_queue] rerere`: gate + driver worktrees run with
      `-c rerere.enabled=true` (shared `rr-cache`); a rerere-resolved merge
      still gates. Fixture: same conflict drains twice, second drain
      auto-resolves.

## 6. jj coexistence (thegn-host)

- [ ] 6.1 `.jj/` detection cached alongside the glyph scan (no subprocess);
      sidebar badge glyph via `caps::active_glyphs()` + a
      `[ui] sidebar_show_*` toggle.
- [ ] 6.2 Staging/commit surfaces show a colocated-repo notice; detached HEAD
      in a colocated repo renders as the working copy, not an error state.
- [ ] 6.3 `auto_fetch` skips colocated repos unless `auto_fetch_colocated`;
      unit-test the skip decision as pure logic.
- [ ] 6.4 doctor line (folds into 4.1).

## 7. Structural diff (thegn-core + thegn-host)

- [ ] 7.1 Pure SGR-subset → styled-cell-run parser in `thegn-core` (new
      module; substrate-free; fixture-driven tests from recorded difft
      output; unknown escapes stripped). 95% gate applies.
- [ ] 7.2 `difft` as a managed tool (`Source::GithubRelease`, pinned,
      `path_fallbacks: ["difft"]`) registered in host `known()`; Probe in
      doctor.
- [ ] 7.3 DiffView modal structural route: off-loop subprocess with width /
      `--color always` / `--background` from theme / byte+graph limits /
      timeout; fallback to internal unified with a notice on any failure.
      Toggle key + new action id.
- [ ] 7.4 `thegn diff --structural` (flag on the existing verb; catalog row
      unchanged).
- [ ] 7.5 Guard test: stageable-diff call sites still pin `SANITIZED_DIFF`
      (`--no-ext-diff`) regardless of `structural_diff`.

## 8. Help + docs

- [ ] 8.1 `docs/help/` page updates claim the new action id and describe the
      new keys' behaviour (help + prose ratchets); keybindings/config
      reference pages regenerate, never hand-edited.

## 9. Validation (once, at the end — not per-edit)

- [ ] 9.1 Re-record any muse snapshot a changed frame invalidates (expected:
      none — no default-config frame changes; verify `panel_git__branches`).
- [ ] 9.2 Run `just ci` (includes `openspec validate --all --strict`).
