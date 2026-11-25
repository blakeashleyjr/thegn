Welcome! This discussion stays pinned so it is the first thing a newcomer
reads. It says what thegn is, where everything lives, and how to get help.

## What thegn is

thegn is a terminal-native Git worktree IDE that is its own terminal
multiplexer. Each git repository is a project, each worktree gets its own
tabs, and the sidebar, diff/PR panel, status bar and pin strip are rendered
by thegn's own in-process compositor — no external multiplexer is involved.
thegn is in public alpha: Linux x86_64 and macOS on Apple silicon are
supported, rough edges are expected, and `KNOWN_ISSUES.md` lists the known
limitations so nothing surprises you.

## Install

The README quickstart covers a Nix install, the release tarball and a source
build. After the first launch, run `thegn doctor`: it prints the resolved
terminal capabilities and sandbox backend, and names the fix when a setting
(such as Option-as-Meta on macOS) is wrong.

## Where things live

- `README.md` — install, quickstart and feature tour.
- `KNOWN_ISSUES.md` — known limitations of the alpha.
- `docs/help/` — the help pages; the same text renders inside thegn (`F1`).
- `CONTRIBUTING.md` — how to build, test and change thegn.
- `SECURITY.md` — how to report a security issue privately.
- `CODE_OF_CONDUCT.md` — applies to every discussion here.

## How to use these Discussions

- **Q&A** — questions about using thegn; mark the reply that answered.
- **Ideas** — proposals to shape together; the ones that stick become
  tracked issues.
- **Show and tell** — setups, themes and workflows; screenshots welcome.
- **General** — anything that does not fit the categories above.
- **Announcements** — releases and news, posted by the maintainers.
- Bug reports do not belong in Discussions: the issue tracker's bug
  template captures the diagnostics a report needs.
- Security reports never go anywhere public: use the private reporting
  link in `SECURITY.md`.

Be kind, assume good faith, and enjoy thegn!
