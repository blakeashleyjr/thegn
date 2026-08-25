# SCM workflow customization — audit, land policies, signing, jj coexistence, structural diff

Linear: THE-30

## Why

THE-30 asks for "deep customization and config support for different source
control workflows" plus an "audit of current source control setup". The audit
(design.md §1) found a git layer that is deliberate and well-seamed but
**opinionated in exactly one shape**:

- **One land strategy, hard-coded.** The merge queue folds every branch as a
  2-parent merge commit (`merge-tree --write-tree` + `commit-tree`,
  `fold.rs:119`), message fixed to `Merge branch '<b>' (fold-actor)`. No
  squash, no rebase/linear-history option — roadmap **T 268** is unstarted.
- **thegn's own commits are never signed.** `commit_tree` deliberately skips
  signing so a daemon fold can't stall on a passphrase prompt — correct for
  solo use, but it means a repo with signed-commit enforcement can never use
  the queue. Meanwhile `snapshot_worktree` (`snapshot_dirty`) _inherits_
  ambient `commit.gpgSign` with no override, so it can hang a background op —
  the exact hazard `[git] override_gpg` exists to prevent.
- **Custom merge drivers, `.gitattributes merge=`, and rerere: zero hits.**
  The object-DB fold may bypass drivers a repo depends on, and recurring
  duplicate-fix conflicts (the dominant drain-conflict class in practice) have
  no rerere story.
- **No per-repo `[git]` overrides.** `[workspace.<slug>]` overlays exist for
  `merge_queue`/`pr_queue` but not for `[git]`, so signing/fetch/strategy
  policy cannot differ between a work repo and a personal one.
- **jj / Mercurial parity** (asked directly in THE-30): roadmap group **AS
  (587–600, 622–630)** plans a full VCS backend abstraction; nothing exists in
  code. The honest scoping argument — including why jj's lack of `git
worktree` support cuts at thegn's core model — is design.md §5.
- **difftastic**: thegn currently _defends against_ external diff drivers
  (`SANITIZED_DIFF` pins `--no-ext-diff` so `git apply` patches stay valid),
  and there is no read-only structural-diff option at all.

## What Changes

1. **Configurable land strategy** — `[merge_queue] land_strategy =
"merge" | "squash" | "rebase"` (default `merge`, today's behaviour), all
   three executed in the object DB with the same gate + CAS guarantees, plus a
   configurable fold-commit message template.
2. **Signing for thegn-created commits** — `[merge_queue] sign_commits`
   (default off) signs fold/land commits, with a non-interactive guard: a
   signing failure classifies as an infrastructure `Error` (never blames the
   branch) and can never prompt. `snapshot_worktree` gains the `override_gpg`
   treatment it is missing. Signing _identity_ (which key, whose GNUPGHOME)
   stays with `add-decoupled-identities` — this change only decides _whether_
   thegn's own commits sign.
3. **Merge drivers + rerere in the fold** — paths governed by a custom
   `.gitattributes` merge driver are folded through the existing
   throwaway-worktree real-merge path (the `regenerate_merge` machinery)
   instead of silently bypassing the driver; opt-in `[merge_queue] rerere`
   shares recorded resolutions across drains via the reused gate worktree.
4. **jj colocation coexistence (not parity)** — detect `.jj/` beside `.git/`,
   report it in `thegn doctor`, badge the worktree, treat detached HEAD as
   normal there, warn on staging surfaces (jj ignores the index), and exclude
   colocated repos from `auto_fetch` by default. The full `VcsBackend` seam
   (AS 587) is explicitly **not** built here; design.md records why.
5. **Structural diff view** — `[git] structural_diff = "off" | "auto" |
"difft"` renders read-only diff surfaces (the `Alt /` DiffView modal and
   `thegn diff --structural`) through difftastic's ANSI output, with `difft`
   acquirable as a managed tool. Stageable diffs keep `SANITIZED_DIFF`
   unconditionally — structural output never feeds `git apply`.
6. **Workflow posture in doctor** — one place that reports: git version vs the
   fold's `merge-tree --write-tree` floor (git ≥ 2.38), non-interactive
   signing readiness, declared custom merge drivers, jj colocation.
7. **Per-workspace `[git]` overlay** — `[workspace.<slug>.git]` resolved by a
   `Config::repo_git(root)` accessor, mirroring `repo_merge_queue`.

Every new key is documented in `config/config.toml.example`; no new
capability-catalog rows are needed (the only external-surface change is a
`--structural` flag on the existing `diff` CLI verb, which stays under that
verb's existing catalog row and scope).

## Impact

- **Roadmap:** completes **T 268** (squash/rebase pre-merge); extends **Y 328**
  (commit signing) to thegn-created commits; records the deferral decision for
  **AS 587–589** (VCS backend abstraction / jj) — coexistence now, seam later;
  relates to **X 314** (weave merge driver — _not_ covered: that is a thegn-
  authored semantic driver, this change only routes _user-declared_ git
  drivers) and **AR 675** (signed tags — not covered).
- **Specs:** `git-backend` (jj coexistence, per-workspace overlay, doctor
  posture), `merge-queue` (land strategy, signing, drivers/rerere), `panel`
  (structural diff view), `managed-tools` (difft tool).
- **In-flight changes:** `add-decoupled-identities` owns signing/SSH/GPG
  _identity_ selection (per-tool `GNUPGHOME`, `GIT_SSH_COMMAND`) — this change
  consumes whatever identity is active and adds no key management (the
  credential-broker work scoped elsewhere is likewise untouched).
  `add-cross-host-merge-queue` and `add-merge-queue-tui` both touch
  `merge-queue`: the new keys are policy inputs to the same driver and must
  surface in that TUI's row detail; strategy/signing semantics do not depend
  on where the drain runs. `add-config-trust-resolution` governs whether an
  untrusted repo-root overlay could ever set these keys — it cannot: all new
  keys live in the _trusted_ user config / `[workspace.<slug>]` layer only.
  `add-viewers-and-quick-open` is file-preview routing (path-keyed
  `preview::route_for`) and is not extended — a diff is not a file, so the
  structural view hangs off the diff surfaces directly.
- **Code (host/svc/core):** `fold.rs` + `plumbing.rs` (strategy, `-S`),
  `integrate.rs` (driver routing, rerere), `config.rs`/`config_defaults`
  (keys + overlay), new pure ANSI→cells parser in `thegn-core` (95% gate),
  `diff_view.rs` + `cmd/diff.rs` (structural route), `managed_tool.rs`
  (`difft`), `cmd/doctor.rs` (posture section), `hydrate.rs`/`sidebar.rs`
  (jj badge).
- **No DB schema change.** New keybind/action for the structural toggle and
  the jj badge glyph go through the help + glyph-literal ratchets
  (`docs/help/` update in the same change).
