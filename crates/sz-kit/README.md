# sz-kit

The **superzej embedding contract**. Sibling TUIs (switchboard, termite-chat,
termite-agent) implement [`AppTile`] so superzej can host them as top-level
**app tabs**, while each still ships as a standalone binary via the
[`standalone::run`] harness.

It depends on neither tokio, termwiz, nor `superzej-core`, so a standalone app
links it without pulling in superzej's stack.

## What it provides

- `AppTile` — the per-frame drive contract (`pump` / `wants_redraw` /
  `handle_input` / `render` + a `ChangeHook` to wake the host loop).
- `InputEvent` / `Key` / `Modifiers` — backend-agnostic input. The host
  translates termwiz; the standalone harness translates crossterm.
- `Theme` — semantic tokens → sRGB with prism defaults that mirror superzej's
  chrome palette, plus `Theme::load_superzej_config()` for standalone runs.
- `standalone::run` (feature `standalone`) — a ~0%-idle crossterm event loop;
  each app's `main` becomes a few lines.

## ratatui version is pinned here

`sz_kit::ratatui` is a re-export. **The host and every app must render through
it**, never a direct `ratatui` dep at a different version. The shared
`Buffer` is then one type everywhere and any drift is a compile error.

## How apps depend on it (lives inside the superzej repo)

`sz-kit` is a member crate of the superzej workspace (`crates/sz-kit`). App
repos consume it as a git dependency on the superzej repo, pinned by tag:

```toml
sz-kit = { git = "https://github.com/blakeashleyjr/superzej.git", package = "sz-kit", tag = "sz-kit-v0.1.0" }
```

When building superzej itself, the workspace `[patch]` redirects that git dep
back to the in-tree path, so one version compiles everywhere:

```toml
[patch."https://github.com/blakeashleyjr/superzej.git"]
sz-kit = { path = "crates/sz-kit" }
```

**Bumping sz-kit** = retag here, then bump the `tag` in each app's manifest and
the app submodule pointer in superzej.

> Accepted cost of living in-repo: a standalone app build clones the superzej
> repo (and, because cargo inits a git dep's submodules, the sibling app
> submodules at their pinned commits) into `~/.cargo/git` before compiling only
> `sz-kit`. Bounded and cached. If it ever hurts, extracting `sz-kit` to its own
> repo changes only the git URL/tag in three app manifests.

[`AppTile`]: src/tile.rs
[`standalone::run`]: src/standalone.rs
