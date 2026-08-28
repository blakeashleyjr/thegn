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
`~/.claude-profiles/`.

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
as a window crosses `warn_percent` and again at `crit_percent`. The knobs are the
same ones `[stats.alerts]` uses and mean the same things:

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

## Model-proxy spend

When the opt-in model proxy is enabled (`[model_proxy] enabled = true`), the
**System ▸ Usage** section and the `Alt-u` overlay grow a **model-proxy spend**
block beside the per-account quota windows: cost and token totals for the
trailing week, with a breakdown by route at full width. It is hydrated off the
event loop from the proxy's audit tables on the same refresh cadence as the
quota gauge, so it never blocks a frame, and it is **absent entirely when the
proxy is disabled**. The numbers are the same rollup `thegn proxy stats` prints
and the daemon's `/stats` endpoint serves — one calculation, three surfaces.

Only metadata is ever recorded: route, backend, model, token counts (including
prompt-cache reads/writes), cost, and timings — **never any prompt or response
text**. Subscription/flat-rate lanes account at `$0`, so the spend figure
reflects only marginal, metered cost. Per-scope budget breaches (`[model_proxy.
budget]`) surface through this same usage-alert path.
