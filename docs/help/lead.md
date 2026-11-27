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

On the dev channel (`THEGN_CHANNEL=dev`) `[lead]` always applies. On stable
the Lead is a **preview**: `[lead]` is ignored unless `preview = true` is set
under it, which the in-app setup writes for you. Until then the sidebar shows
one "Lead — set up" row, on either channel.

## Set it up in the app

Press `Alt i` (or click the Lead footer, or run "lead" from the palette) while
the Lead is not configured, or is configured but not yet logged in or not yet
attached to the daemon: a setup flow opens, one confirm per step. Nothing is
typed in a shell. The sidebar's "Lead — set up" row and the welcome tour's
agents chapter (`l`) open the same flow.

1. **Preview.** On a stable build the flow says the Lead is a preview and that
   setting it up writes `preview = true`. There is no relaunch step: stable
   keeps the Lead once the marker is written.
2. **Agent.** Choose the Lead's agent from the detected agents. Only claude is
   supported today; others are listed as not offered.
3. **Prerequisites.** bubblewrap is required (the flow shows install hints and
   a "check again" row). `pasta` is required too: if it is missing the flow
   shows install hints and a "check again" row, with no host-network option.
4. **Config.** A preview shows the exact `[lead]` (including `preview = true`)
   and `[daemon] enabled = true` lines; confirming writes them to your config
   file, keeping the rest of it.
5. **Daemon.** The daemon must restart to carry the Lead's control listener.
   The flow counts the daemon's live sessions and confirms first, saying how
   many would end (terminals, agent runs and Lead tabs it hosts). If an
   existing Lead tab has lost its listener, selecting Lead routes here and
   offers **restart the daemon and retry the Lead**. Confirming runs
   `thegn daemon stop` off the UI loop, waits for it to finish, then reopens
   the Lead so its attach starts the daemon with the listener.
6. **Login.** Opens a tab running `thegn lead login` (see "Log in"), so it keeps
   every isolation guarantee of that verb. Type `/login` there, exit, then press
   `Alt i` again.
7. **Ready.** A summary of the checks, a row that runs the Lead checks in the
   background and shows the result right here, one row per MCP server in `[lead] mcp_profile` (each opens `thegn
lead login --mcp <server>` in a tab), and **open the Lead**.

Pressing `Alt i` again at any point resumes from what is on disk. The final
screen also has a **disable** row that removes the `[lead]` section after a
confirm; `[daemon] enabled` and the saved login are left alone.

## Configure it by hand

The flow writes ordinary config. To do it yourself, add a `[lead]` section
naming the agent entry to run, for example `[lead] agent = "claude"`. Restart
the daemon after adding it: the Lead needs the daemon and its own control
listener. `[lead] account` is refused for now, and only claude is supported as
the Lead's agent. Without `[lead]` there is no Lead section in the sidebar and
`Alt i` opens the setup flow.

## First run, by hand

On stable, `[lead]` is clamped off unless it carries `preview = true`. Not yet
verified live.

1. Install bubblewrap and `pasta`; the Lead refuses to launch without both.
2. Set `[daemon] enabled = true`, `[lead] agent = "claude"` and, on stable,
   `[lead] preview = true`.
3. Run `thegn daemon stop`, then open thegn. Lead tabs end when the daemon
   restarts, so reopen them.
4. Run `thegn lead login` once (see below).
5. Run `thegn doctor` and read its Lead checks.
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
every workspace, and shows "Lead — Enter to start" until it is first opened (or
"Lead — set up" while no `[lead]` is configured).
The first jump creates the group; later jumps focus it. The Lead is global:
it stays listed when you switch workspace and survives detach and reattach.
If the sidebar row says it is disabled, the one-line reason names the missing
channel support or host prerequisite; press `Alt i` to open setup and resolve
it.

Each tab in the Lead group is a **separate conversation**: open another tab
with the normal new-tab key and it starts its own Lead process.

`lead` is a reserved terminal name, so the new-terminal wizard refuses it.

## The indicator

With `[lead]` configured (on stable, as a preview), the right panel carries a pinned
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
