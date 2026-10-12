# tg-kit

The **thegn embedding contract**. Sibling TUIs implement [`AppTile`] so
thegn can host them as top-level **app tabs**, while each still ships as a
standalone binary via the [`standalone::run`] harness.

By default it depends on neither tokio, termwiz, nor `thegn-core`, so a
standalone app links it without pulling in thegn's stack. The opt-in
`host-presets` feature links `thegn-core` so every named `[theme] preset`
resolves to the host's exact palette.

## What it provides

- `AppTile` — the per-frame drive contract (`pump` / `wants_redraw` /
  `handle_input` / `render` + a `ChangeHook` to wake the host loop).
- `InputEvent` / `Key` / `Modifiers` — backend-agnostic input. The host
  translates termwiz; the standalone harness translates crossterm.
- `Theme` — semantic tokens → sRGB with prism defaults that mirror thegn's
  chrome palette, plus `Theme::load_thegn_config_checked()` for standalone runs: it reads only
  `$XDG_CONFIG_HOME/thegn/config.toml` (<= 256 KiB, regular files only) and
  returns a typed `ThemeLoad` that separates a genuine Prism selection from
  absent/oversize/unreadable/malformed fallbacks and per-field issues.
  `--config`, `THEGN_*`, `THEGN_PROFILE` and local user themes are NOT read;
  use `Theme::load_from_path` for an explicit file.
- `standalone::run` (feature `standalone`) — a ~0%-idle crossterm event loop;
  each app's `main` becomes a few lines.

## ratatui version is pinned here

`tg_kit::ratatui` is a re-export. **The host and every app must render through
it**, never a direct `ratatui` dep at a different version. The shared
`Buffer` is then one type everywhere and any drift is a compile error.

## How apps depend on it (lives inside the thegn repo)

`tg-kit` is a member crate of the thegn workspace (`crates/tg-kit`). App
repos consume it as a git dependency on the thegn repo, pinned by tag:

```toml
tg-kit = { git = "ssh://git@github.com/blakeashleyjr/thegn.git", package = "tg-kit", tag = "tg-kit-v0.1.0" }
```

(thegn is private, so the ssh URL is used — cargo resolves it via the git
CLI / SSH key. `.cargo/config.toml` sets `net.git-fetch-with-cli = true`.)

When building thegn itself, the workspace `[patch]` redirects that git dep
back to the in-tree path, so one version compiles everywhere:

```toml
[patch."ssh://git@github.com/blakeashleyjr/thegn.git"]
tg-kit = { path = "crates/tg-kit" }
```

**Bumping tg-kit** = retag here, then bump the `tag` in each app's manifest and
the app submodule pointer in thegn.

> Accepted cost of living in-repo: a standalone app build clones the thegn
> repo (and, because cargo inits a git dep's submodules, the sibling app
> submodules at their pinned commits) into `~/.cargo/git` before compiling only
> `tg-kit`. Bounded and cached. If it ever hurts, extracting `tg-kit` to its own
> repo changes only the git URL/tag in three app manifests.

[`AppTile`]: src/tile.rs
[`standalone::run`]: src/standalone.rs
