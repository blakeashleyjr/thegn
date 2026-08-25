# Deeper submodule integration in git views

Linear: THE-32

## Why

thegn has essentially **zero** submodule support today — the audit found one
real call site in the whole codebase (`nuke_working_tree` recursing a
`submodule foreach … reset --hard`, `crates/thegn-svc/src/git/branch.rs:101`)
and nothing else. In a repo that uses submodules, that absence is not just a
missing view — it produces wrong behaviour:

- **Line staging can corrupt a gitlink hunk.** `patch.rs` has no `Submodule`
  `FileKind` and parses `-/+Subproject commit <sha>` as ordinary text lines,
  so the staging UI happily offers to stage half a pointer change, which
  `git apply --cached` rejects or misapplies.
- **Fresh worktrees are broken checkouts.** `git worktree add` never
  populates submodules, and `worktree::add` runs no `submodule update
--init` — so every new tab in a submodule repo starts with empty submodule
  directories (and a dirty status), the single most common submodule+worktree
  complaint. Workspace clone (`git clone`) and the remote provision script
  likewise never pass `--recurse-submodules`.
- **Dirty is indistinguishable.** The glyph scan's porcelain-v1 parse keeps
  only two status chars; a dirty submodule shows the same `●` as a file edit,
  and a gitlink change contributes `-\t-` to numstat, so `+N/-N` badges read
  0 for real changes.
- **Diff surfaces are opaque.** A pointer move renders as two meaningless
  SHA lines instead of "what commits moved"; the file explorer draws a
  submodule as a plain directory and descending into it silently crosses
  into another repo.

## What Changes

1. **Correctness first — atomic gitlinks in the patch engine.** `FileKind::
Submodule` (mode 160000 / `Subproject commit` lines) in `patch.rs`; the
   staging surfaces treat a submodule as whole-entry stage/unstage only,
   never line-split.
2. **Lifecycle — populated checkouts.** `[git] submodules = "auto" | "off"`
   (default `auto`: act only when `.gitmodules` exists). Worktree creation
   runs `git submodule update --init --recursive` off-thread after `worktree
add` (usually network-free — worktrees share the superproject's
   `.git/modules`), non-fatal and surfaced on failure; workspace clone and
   the remote provision script gain `--recurse-submodules` under the same
   key. Initialization in an **untrusted** repo requires the same consent
   gate as other repo-driven execution (submodule URLs are repo-controlled
   input) — reconciled with `add-config-trust-resolution`.
3. **Surfacing.** Change rows flag submodules; the diff/preview path renders
   a pointer move as `old → new` plus a bounded `git -C <sub> log --oneline
old..new` summary when the commits are present locally; the sidebar gains
   a distinct submodule-dirty indicator (with a `[ui]` visibility toggle)
   fed by a widened glyph read.
4. **Queue clarity.** A fold conflict on a gitlink is reported as a
   "submodule pointer conflict" naming both commits — and is never
   auto-resolved by thegn or handed to rerere/drivers; pointer choices are a
   human (or explicitly prompted agent) decision.

No new capability-catalog rows: no new CLI verbs or external doors — `wt
new`/clone behaviour changes ride existing verbs, and all new keys are
documented in `config/config.toml.example`.

## Impact

- **Roadmap:** adds a new item to group **Y** (Git integration) — tasks.md has
  no existing submodule item (the only historical mentions are thegn's own
  removed `apps/*` submodules).
- **Specs:** `git-backend` (submodule-aware reads + lifecycle key), `cli`
  (`wt new` initializes submodules), `workspace` (clone recurses), `panel`
  (pointer-move rendering, atomic staging), `sidebar` (submodule-dirty
  indication), `merge-queue` (pointer-conflict reporting).
- **In-flight changes:** `add-config-trust-resolution` (consent gate for
  init in untrusted repos); `stabilize-sidebar-internals` /
  `add-sidebar-actions-and-mouse` (the badge lands in the same glyph
  cluster — coordinate on `GitGlyphs`); `add-scm-workflow-customization`
  (sibling THE-30 change; both touch `git-backend` and `merge-queue` specs
  with disjoint requirements). `add-viewers-and-quick-open` untouched.
- **Code:** `patch.rs` + `forge/model.rs` (gitlink parse), `worktree.rs` +
  `workspace_create.rs` + `remote.rs::provision_repo_script` (lifecycle),
  `git/mod.rs` (`GlyphReads` + a submodule read), `hydrate.rs` (`GlyphRow`
  widening — a **positional tuple**, so `merge_glyph_scan`, its five unit
  tests, warmcache serde, and the DB `glyph_cache` JSON all move together),
  `panel/mod.rs` (`ChangeRow`), `sidebar.rs`/`sidebar_view.rs` (badge),
  `integrate.rs`/`fold.rs` (conflict naming).
- **No SQLite schema change** (`glyph_cache` stores JSON; the widened row is
  additive). New badge glyph goes through `caps::active_glyphs()` and its
  `[ui]` toggle through the generated config reference; muse re-record cost
  is expected 0–1 snapshots (`panel_git__branches` only if header text
  shifts).
