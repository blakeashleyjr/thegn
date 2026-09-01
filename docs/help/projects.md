---
id: projects
title: Programs (multi-repo groups)
order: 3
---

# Programs

A **program** groups several projects (repos) that you work on together — one
feature that spans an `api`, a `web`, and a `shared-lib`. It is a grouping
layer _above_ projects: a project is exactly one repo, and each git worktree
is still a tab. A program records “these repos belong together” and lets you
create a feature across all of them in one command.

Programs are **grouping only — they carry no policy.** Assigning a program
never changes a project's credentials, egress, budget, sandbox, or env
bundles. That sub-scoping is what **zones** do (`thegn zone`), and the two are
independent: a project may be in one zone _and_ one program at once, and a
program may even span zones.

> **Not the tracker “project”.** `[issues] project_key` / `project_id` in your
> config name a project in your issue tracker (GitHub/GitLab/Linear/…). That is
> unrelated to `thegn program`, which groups repos in a multi-repo program.
> The old `thegn project` spelling remains a deprecated compatibility alias
> for this program command during the three-release compatibility window.

## Managing programs

Membership is recorded explicitly — never guessed from a filesystem path.

| Command                              | What it does                                                                                   |
| ------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `thegn program list [--json]`        | List programs and their member counts                                                          |
| `thegn program create <name>`        | Create a program                                                                               |
| `thegn program rename <name> <new>`  | Rename a program                                                                               |
| `thegn program rm <name> [--force]`  | Delete a program (refused while non-empty unless `--force`, which unassigns its members first) |
| `thegn program assign <name> [repo]` | Assign a repo to a program (repo defaults to the current directory's repo root)                |
| `thegn program assign none [repo]`   | Unassign a repo from its program                                                               |

The equivalent `thegn project …` forms still execute the same multi-repo
operation and print a deprecation warning naming `thegn program …`.

## Creating a feature across repos

`thegn wt new <name> --program <p>` resolves **one** linked branch name (your
configured `branch_prefix` + a slug of `<name>`, applied once) and creates that
exact branch plus a worktree in every member repo:

```sh
thegn program create shop
thegn program assign shop /code/api
thegn program assign shop /code/web
thegn program assign shop /code/shared-lib

thegn wt new payments-retry --program shop
# creates branch tg/payments-retry + a worktree in api, web, and shared-lib
```

- **`--repos api,web`** restricts creation to a named subset of members.
- Each member runs the ordinary per-repo create pipeline independently. The
  command prints a **per-member outcome** (`created` / `exists` / `failed`),
  and its exit code is non-zero if any member failed.
- **Nothing is rolled back** when one member fails. Re-run the same command:
  members that already have the branch are reported `exists` and skipped, and
  the failed member is attempted again. Add `--json` for one machine-readable
  object covering every member.

The cross-repo link is **branch-name equality** — there is no super-repo, no
manifest, no stored link. A same-named branch you create by hand (`git
worktree add`) in a member repo simply belongs to the same feature.

## Not yet

The sidebar does not yet group member projects under a program header, and the
merge queue stays strictly per-repo (there is no atomic cross-repo land — two
repos share no transaction, and thegn will never pretend otherwise). An
ordered cross-repo drain (a per-program `land_order`, walked stop-on-failure)
is a designed follow-up, not shipped here.
