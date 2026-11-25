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

## First run

Every `thegn lead` step needs the dev channel, because on stable `[lead]` is
clamped off. Not yet verified live.

1. Install bubblewrap and `pasta` (or set `[lead] network = "host"`).
2. Set `[daemon] enabled = true` and `[lead] agent = "claude"`.
3. Run `THEGN_CHANNEL=dev thegn daemon stop`, then open thegn with
   `THEGN_CHANNEL=dev`. Lead tabs end when the daemon restarts, so reopen them.
4. Run `THEGN_CHANNEL=dev thegn lead login` once (see below).
5. Run `THEGN_CHANNEL=dev thegn doctor` and read its Lead checks.
6. Press `Alt i`.

For rules to fire with no window open, also set `[daemon] headless_producers =
true` (see [[automations]]).

## Log in

`thegn lead login` runs the Lead agent's own login in your terminal, on the
host network, from a fresh temporary directory. It copies back only the
credentials file into the Lead's private home. `thegn lead login --mcp
<server>` does the same for one of the Lead's MCP servers (it must be listed in
`[lead] mcp_profile`), using temporary package caches and copying back only
token files. The Lead cannot influence either login. The verb is refused
when `THEGN_CALLER` or `THEGN_STAGE_LAUNCH` is set, or when stdin is not a TTY.

The Lead keeps a private MCP dir under `$XDG_STATE_HOME/thegn/lead-harness/mcp`
for its package caches and tokens. Your own `~/.mcp-auth` and `~/.npmrc` are
masked inside the Lead.

## Open it

Press `Alt i` (**Jump to the Lead**, `jump-lead`, or run "lead" from the
palette). The `LEAD` section is pinned at the very top of the sidebar, above
every workspace, and shows "Lead — Enter to start" until it is first opened.
The first jump creates the group; later jumps focus it. The Lead is global:
it stays listed when you switch workspace and survives detach and reattach.

Each tab in the Lead group is a **separate conversation**: open another tab
with the normal new-tab key and it starts its own Lead process.

`lead` is a reserved terminal name, so the new-terminal wizard refuses it.

## The indicator

With `[lead]` configured (dev channel only), the right panel carries a pinned
Lead footer at the bottom, whichever tab is open. There is no setting to hide
it. Its height follows the panel:

| Panel size                  | Footer                                     |
| --------------------------- | ------------------------------------------ |
| under 8 rows                | none                                       |
| under 16 rows or 30 columns | 1 row: mark, state, approvals chip, lanes  |
| under 60 columns            | 2 rows: status line, last action and keys  |
| 60 columns and wider        | 3 rows: a `LEAD` header rule, then the two |

```
 LEAD ---------------------------------------
 * working  [2 approvals]  lanes 1/4  spawns 3/20
 PROJ-12 review asked 4m              Alt i
```

The state word is the first of these that applies:

1. **error**: the last launch of the Lead was refused;
2. **needs you**: at least one approval is waiting;
3. **paused**: the kill switch is on;
4. **unknown**: the pause flag could not be read;
5. **working**: a Lead pane printed output on its own within the activity
   quiet grace, or a `lead.wake` automation run is started, unfinished and
   under six hours old;
6. **idle**: the Lead is open and quiet;
7. **not started**: the Lead has not been opened yet.

The approval count covers every repo, the same list `approvals.list` returns;
the chip is amber and inverse, and reads `99+` at the cap. Click it to open
System > Notifications on the first row (approvals sort first) with row mode
on, so `y` and `n` work at once. If that row is not an approval, the request
is in another repo: the status line says so and names `g` to widen the scope.
Clicking anywhere else on the footer jumps to the Lead, exactly like `Alt i`.

The last-action line shows the newest Lead rule or `lead.wake` run (skipped
and dropped runs are ignored), or the newest pending request ("asked
<issue> <stage>") if that is newer. The right side shows `y/n` while you are
needed, `thegn lead resume` when paused or `thegn lead pause` otherwise (wide
footers only), and the live chord for **Jump to the Lead**.

Limits: the footer sees automation rule runs and approval requests only.
Interactive tool calls you make in a Lead tab are not recorded, so they never
appear. The working mark is a static dot, with no spinner. The sidebar `LEAD`
row also gains a suffix for working, needs you, paused, unknown and error
("Lead (1 tab) · working"); it stays "Lead — Enter to start" until first use.

## What it can reach

The Lead only has the tools listed in [[ai-usage]] under "Lead tool
surface", each gated by its scope. All repositories are read-only to it.

## Mail and tasks

The Lead reads your mail and tasks through `notifications.list kind=inbox_item`,
fed by the daemon's native inbox (see [[inbox]]). `producers.status` shows which
producers and inbox accounts are armed and their last error. It does not need
its own MCP servers for Todoist or mail, and never sees the credentials.

## Approvals

The Lead never grants an approval. It asks (`approvals_request`) and you
answer: press `y` or `n` on the request in the Notifications section, check
`thegn lead status` for pending requests, or answer from your phone (see [[mobile-access]]). Calls
that reach outside thegn, such as `issues.comment` and `issues.update`, ask
you first unless the cap is listed in `[lead] outward_allow` (default empty).

## Automation and the kill switch

Rules the Lead writes may only notify, open sessions, wake the Lead or start a
pipeline at `propose`. They are bounded by the `[automations]` budgets, and
`thegn lead pause` stops the Lead's automation and kills the sessions it
started. `thegn lead resume` and `thegn lead status` are described in
[[automations]]. The Lead can pause, but only you can resume.

`merge` autonomy lands unattended only when `[merge_queue] gate_sandbox =
"sealed"` is set and the sealed gate resolves for the repo; otherwise it
behaves like `review` and every land stops for your approval. The key is off by
default. A gate that needs the network or the Nix daemon cannot run sealed, so
those lands still ask. See [[sandboxing]]. `thegn lead pause` also stops
daemon-driven PR queue work.

## The sandbox

The Lead runs in a bubblewrap sandbox: repositories read-only, a private agent
home, thegn state, config and credentials masked, and an isolated network
through `pasta`. It refuses to open without bwrap, or without pasta when the
network is `isolated`. See [[sandboxing]] for the full list.
