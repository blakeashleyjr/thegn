---
id: lead
title: The Lead
parent: workflows
order: 21
actions: [jump-lead]
---

# The Lead

The Lead is a configured agent CLI running in a sandbox that cannot write
code. It watches the whole fleet and drives thegn through a scoped tool
surface: it reads overviews and notifications, opens workers, writes a few
automation rules and asks you for approvals. It never edits a repository and
it never lands work itself.

It needs the **dev channel** (`THEGN_CHANNEL=dev`). On stable, `[lead]` is
ignored and nothing about the Lead appears.

## Configure it

Add a `[lead]` section naming the agent entry to run, for example
`[lead] agent = "claude"`. Restart the daemon after adding it: the Lead needs
the daemon and its own control listener. `[lead] account` is refused for now,
and only claude is supported as the Lead's agent. Without `[lead]` there is no Lead section in the
sidebar and the jump key only says the Lead is not configured.

## Open it

Press `Alt i` (**Jump to the Lead**, `jump-lead`, or run "lead" from the
palette). The `LEAD` section is pinned at the very top of the sidebar, above
every workspace, and shows "Lead — Enter to start" until it is first opened.
The first jump creates the group; later jumps focus it. The Lead is global:
it stays listed when you switch workspace and survives detach and reattach.

Each tab in the Lead group is a **separate conversation**: open another tab
with the normal new-tab key and it starts its own Lead process.

`lead` is a reserved terminal name, so the new-terminal wizard refuses it.

## What it can reach

The Lead only has the tools listed in [[ai-usage]] under "Lead tool
surface", each gated by its scope. All repositories are read-only to it.

## Approvals

The Lead never grants an approval. It asks (`approvals_request`) and you
answer: press `y` or `n` on the request in the Notifications section, use
`thegn approvals`, or answer from your phone (see [[mobile-access]]). Calls
that reach outside thegn, such as `issues.comment` and `issues.update`, ask
you first unless the cap is listed in `[lead] outward_allow` (default empty).

## Automation and the kill switch

Rules the Lead writes may only notify, open sessions, wake the Lead or start a
pipeline at `propose`. They are bounded by the `[automations]` budgets, and
`thegn lead pause` stops the Lead's automation and kills the sessions it
started. `thegn lead resume` and `thegn lead status` are described in
[[automations]]. The Lead can pause, but only you can resume.

In this release `merge` autonomy behaves like `review`: every land stops for
your approval, because the merge gate is not sandboxed yet. Unattended
validation needs a sealed sandbox for the worktree.

## The sandbox

The Lead runs in a bubblewrap sandbox: repositories read-only, a private agent
home, thegn state, config and credentials masked, and an isolated network
through `pasta`. It refuses to open without bwrap, or without pasta when the
network is `isolated`. See [[sandboxing]] for the full list.
