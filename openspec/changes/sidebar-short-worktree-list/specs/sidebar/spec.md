# Sidebar

## ADDED Requirements

### Requirement: Large groups fold to a short list with protected rows

A sidebar group SHALL show at most `sidebar_group_row_limit` entries (default
7, `0` = off) and fold the rest from the tail behind a show-more row. The active
worktree, the cursor row, pinned rows, rows whose attention tier is blocked and
rows whose activity is failed MUST NOT be folded and count toward the limit. A
finished-unread row is not protected.

#### Scenario: Blocked row past the limit

- **WHEN** a group has 20 entries and the 18th is blocked
- **THEN** the 18th is shown and the show-more row counts the hidden remainder

### Requirement: Batch lanes fold into one row per batch

Loose worktrees whose label has a batch token (letters 1-3 then digits 1-6
after the last `/`, followed by `-`) SHALL fold into one collapsed `BatchGroup`
row when at least `sidebar_batch_group_min` (default 2, `0` = off) share it.
Pinned worktrees MUST NOT join. The row opens while it holds the active
worktree, persists its open state as `batch_open:<key>`, counts as one entry
toward its parent's limit, and is not itself limited.

#### Scenario: Two lanes of one batch

- **WHEN** `tg/b158-a` and `tg/b158-b` are loose worktrees
- **THEN** one collapsed `b158 (2)` row replaces them, and `tg/hardy-onyx` stays a normal row

### Requirement: A worktree row never repeats its title or a title counter

The detail line SHALL drop `branch` when it equals the title, `commits` when
the title shows commit or behind badges, `pr` when the title shows the PR chip
and `upstream` when the title shows ahead/behind. A row with nothing left MUST
render as one line, and the heights pass MUST agree with the compose pass.

#### Scenario: Branch equals title

- **WHEN** the worktree `home` is on branch `home`
- **THEN** the row has no second line

### Requirement: The tree guide is continuous through two-line rows and open groups

Ancestor pipes SHALL be drawn in the guide columns of worktree, batch and
show-more rows and their detail lines, and the show-more row MUST close its
group with the corner connector. Folder, Other and Pipelines headers MAY
interrupt the guide.

#### Scenario: Two-line row in a group

- **WHEN** a worktree row in a group renders a detail line
- **THEN** the detail line carries the ancestor pipe in the same column

### Requirement: Worktree names are toned by state

The active worktree name SHALL use the focus tone; a blocked or failed
worktree normal text in bold; working, loading, finished-unread or cursor rows
normal text; seen or idle rows the dim tone. Batch members MUST show their
name without the batch prefix.

#### Scenario: Idle worktree

- **WHEN** a worktree is seen and idle and not under the cursor
- **THEN** its name renders dim
