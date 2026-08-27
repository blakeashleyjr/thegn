---
id: calendar
title: Calendar & clocks
parent: bars
order: 8
actions: [open-calendar]
---

# Calendar & world clocks

`Alt-d` opens the calendar, and so does clicking the date, the clock or the
weather in the top-right corner. It drops down under whichever you clicked.

Three blocks, stacked — four with `[weather]` on: a month grid, the selected
day's agenda, the current weather, and your world clocks. The grid and the
clocks need no configuration beyond a zone name; the agenda depends on having
an event source, and the weather block appears only once you turn `[weather]`
on.

```
╭─ Calendar ───────────────────────────────────────────── esc ─╮
│ ⟨h⟩ August 2026 ⟨l⟩                       today · Fri 21 Aug │
│      Mo   Tu   We   Th   Fr   Sa   Su                        │
│ w31  28   29   30   31    1    2    3                        │
│ w32   4    5    6    7    8    9   10                        │
│ w33  11   12   13●  14   15   16   17                        │
│ w34  18   19   20  [21]  22●  23   24                        │
│ w35  25   26●  27   28   29   30   31                        │
│ w36   1    2    3    4    5    6    7                        │
│                                                              │
│ AGENDA · Fri 21 Aug                                 3 events │
│  09:30-10:00  standup                                   work │
│  all day      ooo: jo                               personal │
│                                                              │
│ WEATHER · Berlin                                             │
│  ☀ Sunny         24°C  feels 25°C  H 26°C L 14°C  41%  8 km/h│
│  Sat             ☀     25°C / 15°C                           │
│  Sun             ☁     19°C / 13°C                           │
│                                                              │
│ WORLD CLOCKS                                                 │
│  local    Fri  21:04  CEST                                   │
│  tokyo    Sat  04:04  JST    +7h  +1d                        │
╰──────────────────────────────────────────────────────────────╯
```

Today is accented and underlined; the selected day wears a highlight. The two
are drawn differently on purpose, so you can still see which day is today after
moving the cursor off it. Days borrowed from the neighbouring months are dimmed,
and a dot marks a day with events.

## Moving around

| Keys                           | Effect                                             |
| ------------------------------ | -------------------------------------------------- |
| `h` `l` or `←` `→`             | previous / next day                                |
| `j` `k` or `↓` `↑`             | next / previous week                               |
| `[` `]` or `PageUp` `PageDown` | previous / next month                              |
| `{` `}` or `Shift-PageUp/Down` | previous / next year                               |
| `g` `G`                        | first / last day of the month                      |
| `t`                            | jump back to today                                 |
| `Tab`                          | move between the grid and the agenda               |
| `Enter`                        | focus the day's events; on an event, open its link |
| `r`                            | re-fetch the visible month                         |
| `Esc` `q`                      | close                                              |

Month arithmetic is instant — the grid never waits on anything, so you can page
through a year as fast as you can hold a key. If you land on a month whose
events aren't cached yet, the grid appears immediately and the dots and agenda
fill in a moment later.

The mouse works too: click a day to select it, click `⟨h⟩` / `⟨l⟩` to page, click
`today` to jump back, and scroll anywhere in the popup to change month.

Paging months from the 31st does what you'd expect rather than what's easy: Jan
31 → Feb 28 → Mar **31**. The clamp to a short month is remembered as temporary.

## World clocks

Add a `[[calendar.clocks]]` entry per zone. Your own zone is always shown first
and doesn't need configuring.

```toml
[[calendar.clocks]]
zone = "Asia/Tokyo"
label = "tokyo"        # optional; defaults to the city from the zone

[[calendar.clocks]]
zone = "America/New_York"
```

Each row shows the weekday, the time, the DST-aware abbreviation (`EST` vs
`EDT`), how far ahead or behind your zone it is, and a `+1d` / `-1d` marker when
it's a different date there.

Offsets are computed fresh every time, never stored, so half-hour zones
(`Asia/Kolkata`, `Asia/Kathmandu`), Lord Howe's 30-minute DST, and the weeks
when two zones disagree about whether summer time has started are all simply
correct. Zone data is compiled into the binary, so this works with no system
`tzdata` — in a container, or on Windows.

A zone name that doesn't exist warns and drops that one clock rather than
failing startup. `thegn config validate` reports it properly, with a suggestion:

```
calendar.clocks[0].zone: unknown IANA time zone "America/New_Yrok",
  did you mean "America/New_York"?
```

## Weather

Off by default. Turn it on and the popup grows a `WEATHER · <place>` block
above the world clocks — condition, temperature, what it feels like, the day's
high and low, humidity, wind, and a short forecast strip:

```toml
[weather]
enabled = true              # the consent step: nothing is fetched until this
location = "Berlin"         # or "" to let the provider infer a city from your IP
units = "auto"              # "auto" follows your locale; or "metric" / "imperial"
show_forecast = true        # the day strip
forecast_days = 3
```

Enabling it is the moment thegn first contacts a weather provider; with
`enabled = false` no thread runs and no request is ever made. The only thing
sent is `location`, and thegn never reads an OS location service.

The block is never a placeholder. Past `stale_after_secs` the heading gains an
age note (`3h ago`) so you know what you're looking at, and past
`hard_expiry_secs` the block — and the `weather` bar widget with it —
disappears rather than show a stale sky. Readings are cached, so the popup has
one the moment it opens after a restart, and the block and the
[`weather` widget](bars.md) always show the same reading.

## Events

Configure one or more `[[calendar.accounts]]`. Sources are read-only — thegn
displays your calendar, it never writes back to it.

```toml
[[calendar.accounts]]
name = "work"                  # also the cache key: unique, and stable
provider = "ics_url"
url = "https://calendar.google.com/calendar/ical/…/basic.ics"
color = "teal"
```

| Provider  | Use it for                                                                                                       |
| --------- | ---------------------------------------------------------------------------------------------------------------- |
| `ics`     | a local `.ics` file, or a **directory** of them — which is the vdir layout `vdirsyncer` and `khal` already write |
| `ics_url` | a subscribed `.ics` / `webcal://` link                                                                           |
| `caldav`  | a CalDAV collection — the only provider with real deltas, so deletions sync too                                  |
| `command` | any program that prints events as JSON — see below                                                               |

There's deliberately no Google or Outlook integration: both need an OAuth client
registration and a consent flow, and both hand out a secret `.ics` URL that
`ics_url` already reads. Fastmail, Nextcloud and Proton do too.

Subscribed URLs are re-fetched conditionally, so an unchanged calendar costs one
`304` and no parsing. CalDAV goes further and asks only for what changed since
last time, which is how a deletion on the server becomes a deletion here rather
than an event that lingers until the next full refetch. Syncs are floored at one
minute apart no matter what `refresh_interval_secs` says.

Events are cached, so they are there the moment the popup opens and survive a
restart. A sync that fails leaves the cache alone — you get yesterday's calendar
rather than an empty one — and a provider that returns an empty calendar when
thegn has events cached is treated as suspect rather than believed.

Recurring events are expanded from their `RRULE` in the event's own timezone,
which is what keeps a weekly 09:00 meeting at 09:00 across a daylight-saving
change instead of drifting an hour twice a year.

## Writing a plugin

A `command` account runs any program and reads newline-delimited JSON from its
stdout. The query arrives in the environment, so a plugin only has to _print_
JSON — never parse it.

```sh
#!/bin/sh
khal list --json "$THEGN_CAL_FROM" "$THEGN_CAL_TO" \
  | jq -c '{method:"events", params:{events:.}}'
```

```toml
[[calendar.accounts]]
name = "khal"
provider = "command"
command = ["thegn-cal-khal"]
capabilities = ["run:khal"]
timeout_secs = 20
```

thegn sets `THEGN_CAL_FROM` and `THEGN_CAL_TO` (as `YYYY-MM-DD`),
`THEGN_CAL_SYNC_TOKEN`, `THEGN_CAL_HOME_ZONE`, `THEGN_CAL_MAX_EVENTS`, and
`THEGN_PLUGIN_API`.

Each output line is `{"method": "...", "params": {...}}`:

- **`events`** — `{events: [...], deleted: [...], sync_token: "..."}`. Send it as
  many times as you like; the pages accumulate. Return the same `sync_token`
  next time to do an incremental fetch.
- **`manifest`** — declare an id, the API version you speak, and the
  capabilities you want. Optional, but it's the only way to request one.
- **`log`** — `{level, message}`, routed to thegn's log. Diagnostics go here or
  on stderr; a stray `echo` on stdout is reported as junk rather than silently
  swallowed.

An event needs only four fields:

```json
{
  "uid": "1",
  "title": "Standup",
  "start": {
    "kind": "zoned",
    "local": "2026-08-21T09:30:00",
    "zone": "Europe/Berlin"
  },
  "end": {
    "kind": "zoned",
    "local": "2026-08-21T10:00:00",
    "zone": "Europe/Berlin"
  }
}
```

`start` and `end` come in three shapes, and the difference matters:

- `{"kind": "date", "date": "2026-12-25"}` — an all-day event. No time, no zone;
  Christmas is Dec 25 wherever you are.
- `{"kind": "zoned", "local": "...", "zone": "..."}` — a wall-clock time. Use
  this for anything recurring, so it holds its local time across DST.
- `{"kind": "instant", "at": "2026-08-21T09:30:00Z"}` — a fixed moment.

Everything else is optional: `description`, `location`, `url`, `calendar`,
`color` (a theme hue name like `"teal"`, never an RGB value), `status`,
`organizer`, `reminders`, and `recurrence` (whose `rules` are plain `RRULE`
strings). Unknown fields are ignored, so a plugin written against a newer thegn
still works against an older one.

Capabilities are `"kind:target"` strings — `"run:khal"`, `"network:example.com"`.
A plugin that asks for something the account didn't grant is denied and the
denial is logged; it isn't fatal. A plugin that runs past `timeout_secs` has its
whole process group killed, and its stderr is kept and surfaced.

## Related

- [Masthead & status bar](bars.md) — the `date`, `clock` and `weather` widgets
  themselves, and their format strings.
- [Configuration](configuration.md) — the full `[calendar]` and `[weather]`
  reference.
