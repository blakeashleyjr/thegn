---
id: ai-usage
title: AI account usage
parent: bars
order: 2
contexts: [panel:usage]
actions: [open-usage]
---

# AI account usage

thegn can track how much of each AI coding account's rate limit you have spent,
when it resets, and warn you before you run out. It reads state the harnesses
already wrote to disk — **thegn never asks for, stores, or transmits an API
key.** Turn the whole thing off with `[usage] enabled = false`.

Three surfaces show the same data, and the same `[usage] warn_percent` /
`crit_percent` thresholds colour all three — a window that is amber on the
gauge is amber in the overlay and the panel too:

- **The statusbar gauge** — `◔ 87% 2h14m`: the single most-consumed window
  across every tracked account, green under 75%, amber past it, red past 90%.
  With more than one account it also names the one that is peaking. Click it, or
  select it and press Enter, to open the overlay.
- **`Alt-u`** (or the palette's _AI account usage_, action `open-usage`) opens
  the overlay: accounts listed **worst first** — the account nearest a limit at
  the top — with one aligned line per limit carrying its bar, the used percent
  and the `resets in …` countdown, the identity facts summarised on one line
  below the numbers, and a legend on the last row.
- **System ▸ Usage** in the panel is the docked version. It widens: at the
  resting width one row per account showing its worst limit, at half width
  every limit, and at full width the account's identity as a single facts line
  below the numbers plus a legend. Window names read in plain language —
  `7-day window`, `5-hour window` — rather than provider shorthand. `r`
  re-gathers now instead of waiting out the poll interval.

The ordering is by how close each account is to a limit, so the list is a
ranking, not a fixed roster: an account that heats up moves to the top.

## Several accounts on one machine

Both Codex and Claude Code locate their _entire_ credential home from a single
environment variable — `CODEX_HOME` and `CLAUDE_CONFIG_DIR`. That is how one
machine comes to hold many logins side by side: thegn's own
[account switcher](configuration.md) works by pointing those variables at
different directories, and Claude Code's profile convention parks them under
`~/.claude-profiles/`. Two accounts that resolve to the same credential
directory are refused rather than treated as separate logins. This includes
existing directories reached through a symlink or a path containing `..`; thegn
warns which account it refused.

So usage is tracked **per credential home**, not per harness. thegn finds them by
looking at:

1. the harness's own default home (whatever `CLAUDE_CONFIG_DIR` / `CODEX_HOME`
   currently points at, else `~/.claude` / `~/.codex`) — **only when you have
   not configured a `dir` for that harness** (see below);
2. every immediate child of `[usage] profile_roots` (default
   `~/.claude-profiles`) that holds the harness's auth marker — either at the
   child itself, or one level in;
3. any `[[usage.accounts]]` you configure.

Each account is then identified from its own `.claude.json`, so rows are labelled
`you@example.com (Your Org)` rather than eight identical "Claude"s. Two paths to
the same login collapse into one row.

`[[usage.accounts]]` exists to **add** a home the scan wouldn't find, **rename**
one whose derived label is unhelpful, or **exclude** one:

```toml
[[usage.accounts]]
name = "work"
provider = "claude"
dir = "~/.claude-profiles/work/.claude"
label = "Work (Acme)"

[[usage.accounts]]
name = "scratch"
provider = "claude"
dir = "~/.claude-profiles/test1/.claude"
enabled = false          # found by the scan, but don't track it
```

**Explicit beats implicit.** As soon as one enabled entry names a `dir` for a
harness, your entries (plus the profile scan, if it is on) are the whole story
for that harness — its default home is no longer added on top. Naming where a
harness lives and then also reading whatever `~/.claude` happens to be would
count the same sessions and tokens twice. List the default home yourself if you
want it alongside your other entries. An `enabled = false` entry doesn't count:
it excludes one home, it doesn't say where the harness lives.

## Where the numbers come from

**Codex** publishes its rate-limit snapshot to disk, in the newest
`~/.codex/sessions/**/rollout-*.jsonl` — the same numbers `/status` shows. It is
read offline, with no network access at all, and carries its own cumulative token
counters.

**Claude** and **Antigravity** publish no window state to disk. Their windows
require one lightweight authenticated request per account, using the OAuth token
already sitting in the credential home. That is why `[usage] allow_network`
defaults to `true` — set it to `false` and no request is ever made, but every
Claude account will read _unavailable_ because there is genuinely nothing local
to read.

A row that can't be read says why: _not logged in_, _token expired_, _network
off_, _no sessions_. Accounts are independent, so one failing never hides the
rest.

Polling runs off the event loop on `[usage] poll_interval_secs` (default 300,
floored at 60), on the blocking pool, so it never touches the event loop. A poll
that returns unchanged numbers repaints nothing.

## Warnings

`[usage.alerts]` raises a toast — and, by default, a notification-inbox entry —
as a window crosses `warn_percent` and again at `crit_percent`. The inbox keeps
one entry per account, window, alert level and reset period; repeats update its
message in place without changing its read state. Dismissing or deleting a row
stays in effect until the window resets or usage retreats below the clear line,
which re-arms the alert even when clear notifications are disabled. A one-poll
gap in reset metadata preserves the last known period. `repeat_secs` controls
in-app toast reminders; inbox repeats still update the existing row. The other
knobs are the same ones `[stats.alerts]` uses and mean the same things:

- `sustain_secs` — how long a crossing must persist before it fires.
- `repeat_secs` — how often a standing alert reminds you. `0` never repeats.
- `clear_margin` — how far a window must retreat before the alert clears, so one
  hovering on the line cannot flap.
- `notify_clear` — announce the recovery too (usually the window resetting).

Leave `used` at zero and the alert lines inherit `warn_percent` / `crit_percent`,
so the thresholds you are warned at and the colours you are looking at cannot
drift apart.

## Token counts are host-wide

With `[usage] token_rollups` on, thegn also totals the tokens in the harnesses'
local transcripts. These are reported **host-wide and never filed under an
account**, because they cannot honestly be attributed to one: transcript records
carry no account or organisation field, and profiles frequently share a single
transcript directory. The per-account percentages above come from the providers
themselves and _are_ per-account accurate; the token totals are a separate,
coarser number and are labelled as such.

## From the command line

`thegn usage [--json] [--tokens]` prints the same per-account snapshot the
overlay shows, without the UI. The default is an aligned table: provider,
account, plan, state, the busiest window's `used %`, and its reset countdown
(an unavailable account shows its note instead). It gathers exactly as the UI
does, so `[usage] enabled` and `[usage] allow_network` apply; there is no
separate live-fetch flag.

`--tokens` appends the host-wide token totals, with `(floor: N transcripts
skipped)` when the transcript cap was hit and the totals are a lower bound.
`--json` emits one document with `generated_ms`, `enabled`, `accounts[]`
(`provider`, `account`, `plan`, `state`, `note`, `email`, `org`, `home`,
`windows[]` of `label` / `used_pct` / `resets_at`) and `tokens`. With
`[usage] enabled = false` the command prints
`usage tracking is off ([usage] enabled = false)` (JSON: `enabled: false`,
empty `accounts`) and exits 0.

## Model-proxy spend

The model proxy is a separate binary, `tgproxy`. The alpha release tarball and
nix flake ship only `thegn`: build the proxy from a source checkout with
`cargo install --path crates/thegn-proxy` and put it beside `thegn` or on
`PATH`. `thegn doctor` reports where it found `tgproxy`, or `not installed`.
A non-executable `tgproxy` beside `thegn` is skipped in favour of `PATH`, and
doctor names the skipped sibling. `thegn doctor --json` reports it as
`model_proxy.tgproxy_skipped` (null when none is skipped).

When the opt-in model proxy is enabled (`[model_proxy] enabled = true`), the
**System ▸ Usage** section and the `Alt-u` overlay grow a **model-proxy spend**
block beside the per-account quota windows: cost and token totals for the
trailing week, with a breakdown by route at full width. It is hydrated off the
event loop from the proxy's audit tables on the same refresh cadence as the
quota gauge, so it never blocks a frame, and it is **absent entirely when the
proxy is disabled**. The numbers are the same rollup `thegn proxy stats` prints
and the daemon's `/stats` endpoint serves — one calculation, three surfaces.

While thegn's UI runs, it supervises the proxy. Editing `[model_proxy]` applies
on config reload: `enabled = false` stops the proxy it is supervising, enabling
starts it, and any other change restarts it. Requests in flight during a restart
are cut and retried by the agent's client. If a reload moves `listen` to an
address held by a foreign or incompatible listener, or the proxy secret cannot
be read, the running proxy stays at its old address while the supervisor
retries on its normal backoff. The reason is logged once per change under
`thegn::proxy`; it is not shown in the UI. Keeping the same `listen` address
does not trigger this check.

Only metadata is ever recorded: route, backend, model, token counts (including
prompt-cache reads/writes), cost, and timings — **never any prompt or response
text**. Subscription/flat-rate lanes account at `$0`, so the spend figure
reflects only marginal, metered cost. Per-scope budget breaches (`[model_proxy.
budget]`) produce one deduplicated `UsageLimit` notification for each breached
scope, rolling-window anchor, and token/cost dimension. These configured spend
caps are separate from the provider quota windows and `[usage.alerts]` warnings
described above; no new usage-budget key is involved.

Upstream calls have a 10-second connection limit. Buffered calls must receive
headers within `[model_proxy] first_byte_timeout_secs` (45 seconds by default),
receive each body chunk within `idle_timeout_secs` (120 seconds by default),
and finish within 300 seconds total. Streams share the first-byte limit across
opening headers and the first usable output, then use the idle limit between
chunks; they end after one hour even while data continues. A timed-out attempt
can fall through to the next upstream lane.

### Metered AI spend in the fleet meter

The masthead `fleet` cost meter adds the proxy's metered spend to provider VM
burn, so one number covers both: `$/h` is VM burn plus the cost of requests in
the trailing hour. Only pay-per-token usage counts (API keys through the model
proxy); subscription and OAuth lanes cost `$0` here and stay in the usage `%`
gauge. The `fleet` popup adds a **metered AI** block with the burn, today (UTC)
and month to date, split per backend/model. Costs the proxy computed from its
price table are estimates, and the popup says so; a request with usage but no
known price is not counted and the popup notes how many. With the model proxy
disabled, or nothing recorded, there is no AI spend and the meter shows VM burn
only. See [bars](bars.md).

## MCP tools

### Lead tool surface

`thegn mcp serve` exposes eleven tools for the Lead, each gated by the scope in
the table. Every one needs a running daemon. Granting an approval
(`approvals.resolve`) is admin-only and is never an MCP tool: it stays on the
HTTP API and `thegn api call`.

| Tool                 | Scope   | What it does                                                       |
| -------------------- | ------- | ------------------------------------------------------------------ |
| `overview_get`       | `read`  | One cross-workspace overview: worktrees, agents, queues, attention |
| `notifications_list` | `read`  | Notifications filtered by kind, unread state, age and worktree     |
| `notifications_ack`  | `write` | Mark one notification, a worktree's, or all of them read           |
| `automations_put`    | `write` | Create or replace a Lead-authored automation rule                  |
| `automations_rm`     | `write` | Remove a Lead-authored rule (config rules refuse)                  |
| `automations_enable` | `write` | Enable or disable a Lead-authored rule (config rules refuse)       |
| `automations_runs`   | `read`  | Recent audited runs of an automation rule                          |
| `automations_pause`  | `write` | Pause all automation (the kill flag); only you can resume          |
| `approvals_list`     | `read`  | Approval history and pending approval requests                     |
| `approvals_request`  | `write` | Ask a human to approve a pipeline stage (never grants)             |
| `merge_drain`        | `git`   | Start the merge-queue drain for a worktree's repository            |

`merge_drain` is gated like `merge_add`: the narrow `merge_add` grant belongs to
worktree-bound route tokens, so an MCP caller needs `git`. A Lead never gets
either: it lands only through a rule's land step or an owner approval, so a
Lead-confined `merge_add` or `merge_drain` is refused whatever its scopes.
Rules the Lead writes may only notify or open sessions.

### The overview reply

`overview.get` takes `{ "workspace": "<repo root>", "limit": 50 }`, both
optional (`limit` defaults to 50, caps at 500, and bounds every section). The
reply carries:

- `worktrees`: each registered worktree with its attention `tier`
  (`blocked`, `failure`, `waiting`, `ready`, `working`, `idle`), the `reason`,
  `since` (epoch seconds) and `unread` count, most urgent first.
- `roster`: active agent dispatches grouped by pipeline stage.
- `merge_queue` and `pr_queue`: queued entries that have not landed.
- `ci_failing`: worktrees whose latest cached CI run failed.
- `approvals_pending`: approval requests nobody has answered.
- `notifications`: unread total and a count per kind.
- `calendar_today`: today's events in your home zone.
- `partial`: one `"section: reason"` string per source that failed or is
  approximate. A failing source never fails the call.

The view is read from the database and caches only. Live state that exists
only in the UI, such as which pane an agent has focused, is not visible, so
activity is approximate and `partial` always says `activity: daemon-only view`.

### Driving sessions over MCP

`sessions_input` takes exactly one of `text`, `bytes_b64` or `keys`. `keys` is
an array of at most 64 key names, in the syntax of `thegn session send --key`
(see [[daemon-and-sessions]]). `paste: true` goes with `text` only and sends it
as a (bracketed, when the pane enabled it) paste. Authorisation is unchanged:
`write` scope **and** `--allow-session-input`. `sessions_snapshot` also returns
`seq`, `cursor`, `alt_screen`, `app_cursor`, `bracketed_paste`, `mouse`,
`sgr_mouse` and `title`, and refuses a
session id containing `/ ? # %` or equal to `.` or `..`. `sessions_wait` takes
`screen:<regex>` and `stable:<ms>` as well.

### Wiring the Lead

When `[lead]` is configured, the daemon opens a second control listener on
`127.0.0.1` with a random port. It accepts bearer tokens only: unlike the unix
socket, it never treats a same-user caller as admin. The daemon writes the port
to `$XDG_STATE_HOME/thegn/lead-control/control-addr.json`. Without `[lead]`, it
opens nothing.

Each time a Lead tab starts, thegn does three things:

1. It revokes the previous token of the same Lead tab.
2. It mints a new token for the Lead. The token's scopes are `[lead] scopes`,
   clamped by `[mcp.serve] scopes`. The Lead can never hold more than
   `[mcp.serve]` allows, and it never holds `admin` or `exec`.
3. It writes the token into `$XDG_STATE_HOME/thegn/lead-control/mcp-<tab>.json`,
   an owner-only file. Claude reads it with `--mcp-config ... --strict-mcp-config`,
   so your own `~/.claude` MCP settings never apply to the Lead.

`mcp.json` registers two servers:

- `thegn`: `thegn mcp serve`, which reaches the daemon through the Lead
  listener. `sessions_input` is never enabled.
- `thegn-lead-hub`: `thegn mcp proxy --profile <[lead] mcp_profile>`. It is
  registered only when that profile names at least one server.

The servers listed in `[mcp_proxy.profiles]` are private to their profile. The
default `thegn mcp proxy`, which every worker agent uses, leaves them out. An
unknown `--profile` is an error. A server inside a profile still needs its own
`proxy.tools` list, the same as any other server.

```toml
[mcp_proxy.profiles]
lead = ["todoist"]
```

A profile's secrets come from the daemon, and only the Lead's token gets them.
`thegn mcp proxy --profile <p>` does not read the profile's keyring entries or
secret files itself. It asks the daemon (`mcp_proxy.profile_env`) with the
`THEGN_CONTROL_ADDR` and `THEGN_CONTROL_TOKEN` from `mcp.json`, and the daemon
answers only:

- a Lead token, for `[lead] mcp_profile` only;
- an admin caller, for any profile.

Every other caller is refused, and so is a failed request: there is no local
fallback. `--profile` is also refused outright inside a pipeline stage
(`THEGN_STAGE_LAUNCH` is set) or for an `agent:` caller. A worker has no Lead
token, so it cannot get the profile's secrets through thegn. The daemon logs
the profile name only, never a value.

The Lead's token expires. `[lead] token_ttl_secs` (default 43200, 12 hours;
300 to 604800) sets its lifetime from the moment the tab starts. While the
Lead keeps using it, the expiry slides: when less than half the TTL is left,
the next request moves it to now + TTL, at most 7 days after the token was
minted. A token that sits idle for a whole TTL stops working, and the Lead's
calls then fail with 401. Reopen the Lead tab: it mints a fresh token. A
revoked or expired token never slides back to life.

While `[lead]` is configured, every pairing code redeemed on the
`thegn serve` listener waits for your approval (`thegn pair approve`), as if
`[serve] require_approval` were on. The Lead could otherwise see a pairing code
in a pane and pair itself.

The Lead needs the daemon. If the daemon is off, or `[lead]` was added after
the daemon started, opening the Lead fails with a hint to restart the daemon.
Only claude is supported as the Lead's agent so far.

### What the Lead may ask the daemon to do

A Lead token is confined whatever its scopes. In short:

- `sessions.open` launches only a configured agent, by name, in a registered
  worktree whose launch resolves to a real sandbox. No raw `argv`, no `env`.
- `sessions.input` works only into a live worker a Lead opened, and only as
  plain text plus Enter (no escape sequences or other control characters, at
  most 16 KiB). The MCP tool also needs `--allow-session-input`, which the
  Lead's wiring does not pass today. Named keys and paste are refused for a
  Lead too.
- `sessions.attach`, `sessions.split`, `sessions.fork`,
  `git.stage`, `git.commit`, `worktrees.open`, `worktrees.create`,
  `editor.open`, `worktrees.sandbox_profile_set` and every admin or exec verb
  are refused.
- `merge.add` and `merge.drain` are refused: the Lead lands only through a
  rule's land step or an owner approval.
- Reads, `sessions.kill`, the tracker, dispatches (without binding a session),
  notifications, approvals requests, its own rules and reading the merge queue
  work as their scopes say.

The full list is in [sandboxing](sandboxing.md#what-the-lead-may-ask-the-daemon-to-do).

To debug the wiring, `thegn mcp wire --lead` writes `mcp.json` and prints its
path without starting anything. Because it mints a new token, a Lead that is
already running must be reopened. `thegn mcp wire --lead --remove` deletes the
file and revokes every Lead token.
