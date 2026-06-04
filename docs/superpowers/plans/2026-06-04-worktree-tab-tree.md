# Worktree tab tree — implementation plan

Spec: `docs/superpowers/specs/2026-06-04-worktree-tab-tree-design.md`
Status: implemented + verified 2026-06-04

## Steps

1. **Shared parsing** — DONE: `split_page(branch) -> (base, page)` added to
   both plugin crates (mirrors the binary's `strip_page_suffix`; digits-only
   suffix rule), with unit tests (host-runnable where pkg-config exists; the
   rule is also covered by the binary's `strip_page_suffix` tests).
2. **Sidebar** (`plugin/sidebar`) — DONE: `RepoView.worktrees` →
   `WorktreeView { label, pages, active }` / `PageView { n, position,
   active }`; home first, then worktrees by lowest tab position; pages by
   number. Rows: `Repo | Worktree | Page | AddWorktree | AddNew`; page rows
   only when `pages.len() > 1`. Worktree row → page-1 tab (lowest if ·1
   closed); page row → its tab; repo row unchanged. Tree connectors:
   `├/└` worktrees, `│   ├/└ ·N` pages (trunk blank under last worktree).
3. **Tabbar** (`plugin/tabbar`) — DONE: strip shows only the focused
   worktree's pages (`active_wt = (repo, base)` from the active tab); chips
   ` 1 `, ` ·2 `, … sorted by page, same span/click/hover mechanics.
4. **Tests** — DONE: `test/nav-ux.py` "sidebar tree navigation" section
   drives the sidebar by keys (Alt+h, j…, Enter) and asserts: home row →
   home tab, worktree row → base tab, page ·2 row → its tab.
5. **No binary / DB / layout / keybind changes.**

## Verification log (2026-06-04)

- `python3 test/nav-ux.py` → PASS ×2 (30 assertions incl. 4 new tree-nav)
- `test/one-session.sh` PASS, `test/smoke.sh` all green
- `cargo test --release` 9 passed; clippy clean for binary + both plugin
  crates (wasm32-wasip1); fmt applied
