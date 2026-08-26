---
id: projects
title: Projects (multi-repo groups)
order: 3
---

# Projects

A **project** groups several workspaces (repos) that you work on together —
one feature that spans an `api`, a `web`, and a `shared-lib`. It is a grouping
layer _above_ workspaces: a workspace is still exactly one repo, and each git
worktree is still a tab. A project just records "these repos belong together"
and lets you create a feature across all of them in one command.

Projects are **grouping only — they carry no policy.** Assigning a project
never changes a workspace's credentials, egress, budget, sandbox, or env
bundles. That sub-scoping is what **zones** do (`thegn zone`), and the two are
independent: a workspace may be in one zone _and_ one project at once, and a
project may even span zones.

> **Not the tracker "project".** `[issues] project_key` / `project_id` in your
> config name a project in your issue tracker (GitHub/GitLab/Linear/…). That is
> unrelated to `thegn project`, which groups repos in your workspace.

## Managing projects

Membership is recorded explicitly — never guessed from a filesystem path.

| Command                              | What it does                                                                                   |
| ------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `thegn project list [--json]`        | List projects and their member counts                                                          |
| `thegn project create <name>`        | Create a project                                                                               |
| `thegn project rename <name> <new>`  | Rename a project                                                                               |
| `thegn project rm <name> [--force]`  | Delete a project (refused while non-empty unless `--force`, which unassigns its members first) |
| `thegn project assign <name> [repo]` | Assign a repo to a project (repo defaults to the current directory's repo root)                |
| `thegn project assign none [repo]`   | Unassign a repo from its project                                                               |

## Creating a feature across repos

`thegn wt new <name> --project <p>` resolves **one** linked branch name (your
configured `branch_prefix` + a slug of `<name>`, applied once) and creates that
exact branch plus a worktree in **every** member repo:

```sh
thegn project create shop
thegn project assign shop /code/api
thegn project assign shop /code/web
thegn project assign shop /code/shared-lib

thegn wt new payments-retry --project shop
# creates branch tg/payments-retry + a worktree in api, web, and shared-lib
```

- **`--repos api,web`** restricts creation to a named subset of members.
- Each member runs the ordinary per-repo create pipeline independently. The
  command prints a **per-member outcome** (`created` / `exists` / `failed`),
  and its exit code is non-zero if any member failed.
- **Nothing is rolled back** when one member fails. Just **re-run the same
  command**: members that already have the branch are reported `exists` and
  skipped, and the failed member is attempted again — so a partial failure is
  always completed by retrying. Add `--json` for one machine-readable object
  covering every member.

The cross-repo link is **branch-name equality** — there is no super-repo, no
manifest, no stored link. A same-named branch you create by hand (`git worktree
add`) in a member repo simply belongs to the same feature.

## Not yet

The sidebar does not yet group member workspaces under a project header, and
the merge queue stays strictly per-repo (there is no atomic cross-repo land —
two repos share no transaction, and thegn will never pretend otherwise). An
ordered cross-repo drain (a per-project `land_order`, walked stop-on-failure)
is a designed follow-up, not shipped here.
