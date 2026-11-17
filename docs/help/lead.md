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
surface: it reads overviews and notifications, and queues approvals and
merge drains. It never edits a repository.

It needs the **dev channel** (`THEGN_CHANNEL=dev`). On stable, `[lead]` is
ignored and nothing about the Lead appears.

## Configure it

Add a `[lead]` section naming the agent entry to run, for example
`[lead] agent = "claude"`. Without `[lead]` there is no Lead section in the
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
