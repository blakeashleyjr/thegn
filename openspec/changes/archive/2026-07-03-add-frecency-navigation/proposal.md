# Add frecency navigation (fast workspace entry + connect-to-root)

## Summary

Make landing in the right worktree instant, borrowing the entry-UX from
[`sesh`](https://github.com/joshmedeski/sesh). Four additions, all in the shell
(no AI):

1. **Frecency-ranked opener** — a palette mode that lists repos/worktrees ranked
   by a combined recency×frequency score (not raw recency), so the place you go
   to most floats to the top.
2. **Connect-to-root** — from any nested cwd (a shell pane deep in a subdir),
   one action jumps to the owning worktree's tab, resolving the worktree root via
   git (git is already the source of truth).
3. **Clone-and-open** — one action clones a URL, registers it as a workspace, and
   opens its first worktree tab.
4. **Layout import** — a pure parser that reads a `tmuxinator`/`sesh` project file
   and offers it as a worktree/layout source, lowering migration cost.

## Impact

- **C 40** (recent/favorite workspaces) — upgrades raw-recency ordering to a
  frecency score; **D** (worktrees) — connect-to-root + clone-and-open create and
  reveal worktree tabs.
- **cmd+k palette** — a new palette mode over repos/worktrees; reuses the nucleo
  matcher + `FileIndex` session.
- Extends the `command-palette` and `navigation` capabilities. **No DB schema
  change** — the `frecency` table already exists; ranking is a read-time score.

## Rationale

sesh's whole value is "type a fuzzy fragment, land in the right session," backed
by zoxide frecency and a `sesh root` jump. thegn already persists `repos`
(`last_opened`, `seq`, `open_count`) and a `frecency` table for the palette, and
already knows every worktree root — so this is wiring existing data into the
opener, not new persistence. `sesh root` in particular maps cleanly: a user who
`cd`s deep into a worktree in a shell pane should be able to snap the sidebar/tab
focus back to that worktree in one key. Importers (tmuxinator/tmuxp/sesh) are a
migration lever: they let users bring existing layouts without hand-rebuilding.

## Non-goals

- **A zoxide hard dependency** — thegn owns its own frecency table; reading an
  external `zoxide query` is an optional enrichment, not required.
- **A general session-manager mode** — thegn is one-session (repos/worktrees
  are tabs); this is about _entering_ a worktree fast, not spawning detached
  sessions.
- **Bidirectional layout export** — importers are read-only; thegn does not
  write back tmuxinator/sesh files.
- **Any AI dependency** — pure shell navigation; no proxy/agent involvement.
