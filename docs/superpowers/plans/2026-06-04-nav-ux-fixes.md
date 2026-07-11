# Navigation UX fixes — implementation plan

Spec: `docs/superpowers/specs/2026-06-04-nav-ux-fixes-design.md`
Status: implemented + verified 2026-06-04 (all steps done)

## Steps

1. **Keybinds** (`config/zellij.kdl`) — DONE
   - `Alt h/j/k/l` → `MoveFocus` (rebound from default `MoveFocusOrTab`).
   - `Super Alt h/l` added for parity with arrows/`j/k`.
   - `Alt t` → `Run thegn new-tab` (floating, close_on_exit).
   - `keybinds.tab` `n` → same Run + `SwitchToMode "Normal"`.
2. **`thegn new-tab`** — DONE
   - `src/commands/new_tab.rs`: derive base tab name from cwd
     (`{slug}/home` or `{slug}/{branch}`), pick lowest free `·N` (N≥2),
     `new-tab --layout worktree-tab-extra` (fallback bare).
   - `strip_page_suffix()` + unit tests; used by `resolve.rs` so the panel
     resolves `·N` tabs to the base tab's worktree.
   - CLI wiring (`cli.rs`, `main.rs`, `commands/mod.rs`), palette entry
     (`menu.rs`), status hints (`status.rs`), README key table.
3. **Layouts** — DONE
   - New `layouts/worktree-tab-extra.kdl` (chrome wrapper, plain-shell center).
   - `tab_template "chrome"` + 3 `swap_tiled_layout` variants
     (vertical | horizontal | stacked), `min_panes=6`, appended to all four
     tab layouts (kept in sync).
4. **Packaging** — DONE: `install.sh`, `nix/package.nix`, `nix/hm-module.nix`
   ship `worktree-tab-extra.kdl`; managed config `~/.thegn/zellij.kdl`
   refreshed (it was the unmodified previous default).
5. **Tests** — DONE: `test/nav-ux.py` (headless pty client; 24 assertions:
   focus-after-create, Alt+h/l nav + edge no-spill, Alt+t `·2`/`·3` chrome
   tabs, tab-mode `n` repoint, resolve `·N`, Alt+] swap cycling with chrome
   pinned, Ctrl+Alt+s toggle-restore regression with 1 and 2 center panes).
   Existing `test/one-session.sh`, `test/smoke.sh`, `cargo test`, clippy,
   fmt all pass.

## Verification log (2026-06-04)

- `python3 test/nav-ux.py` → PASS (24/24)
- `bash test/one-session.sh` → PASS
- `bash test/smoke.sh` → all smoke checks passed
- `cargo test --release` → 9 passed
- `cargo clippy --release` / `cargo fmt --check` → clean
