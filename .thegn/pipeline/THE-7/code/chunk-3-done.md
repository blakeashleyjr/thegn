# THE-7 chunk 3 completion

Implemented the CLI, completion, configuration documentation, and help scope
for the theme builder popup.

## Changes

- Replaced the interactive `fzf`/`gum` theme command with deterministic
  `theme list`, `theme set <name>`, and local Gogh `theme import <file>
[--name]` commands.
- Added bounded local user-theme discovery with built-in-name precedence,
  validated atomic theme saves, real `[theme].preset` persistence, and user
  theme color/hue override persistence.
- Added merged built-in/local theme candidates and catalog classification for
  every new value-taking CLI argument without changing the completion ratchet.
- Added the registered `theming` help page, index link, complete popup keyboard
  model, CLI/import documentation, and user-theme extension guidance.
- Updated only the existing `[theme].preset` example comment to mention local
  theme names and directory.

## Verification

- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp TMPDIR=/tmp just quick thegn-core` — passed.
- `RUSTC_WRAPPER= XDG_RUNTIME_DIR=/tmp TMPDIR=/tmp just quick thegn-host` — passed.
- `cargo nextest run -p thegn-host theme` — 5 passed.
- `cargo nextest run -p thegn-host completion` — 15 passed, including the
  completion-slot ratchet.
- `cargo nextest run -p thegn-host help` — 75 passed, including help and
  environment-related ratchets.
- `cargo nextest run -p thegn-svc control_wire` — 1 passed; control schema
  snapshot unchanged.
- `cargo nextest run -p thegn-core completion` — 42 passed.
- Manual `theme list` invocation passed with fresh temporary
  `XDG_STATE_HOME` and `XDG_CONFIG_HOME` directories.
- `cargo fmt --all -- --check` and `git diff --check` — passed.

## Unverified

- Full-workspace gates (`just test`, `just ci`, coverage, full builds), e2e,
  and deferred visual snapshot cases were not run per the chunk policy.
- Manual import/set persistence was not exercised against a live thegn process;
  no live state database was used.

## Commits

- Early checkpoint: `c0f2b08f wip(the-7): wire deterministic theme CLI`
- Final code commit: `9f132836 docs(the-7): wire theme CLI completion and help`
