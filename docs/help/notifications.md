---
id: notifications
title: Notifications
order: 32
parent: configuration
---

# Notifications

The `[notifications]` section controls the in-app notification inbox, badges,
and optional desktop notifications.

### `agent_error_signatures`

A list of case-insensitive substrings. When any line of live agent output
contains one of these, the worktree's error glyph lights up. The glyph clears
automatically as soon as the agent produces output with no matching line (for
example, when it resumes working).

Defaults:

| Signature            | What it catches                   |
| -------------------- | --------------------------------- |
| `weekly limit`       | Weekly usage cap                  |
| `rate limit`         | Rate-limited API response         |
| `usage limit`        | Generic usage cap                 |
| `limit reached`      | Catch-all limit message           |
| `quota exceeded`     | Cloud quota exhausted             |
| `connection error.`  | Network failure (note the `.`)    |
| `connection refused` | TCP RST                           |
| `network error`      | Generic network fault             |
| …                    | Other configured harness failures |

Set this to `[]` to disable text-based error detection entirely.

### Outbound chat sinks

The optional `[notifications.push]` channel can deliver routed notifications
to ntfy, a generic JSON webhook, Discord, or Slack. The legacy scalar table
remains valid and materializes one sink named after its `kind`. For more than
one destination, add named nested tables:

```toml
[[notifications.push.sinks]]
name = "oncall"
kind = "slack"
url = "env:THEGN_SLACK_ONCALL_URL"
min_priority = "alert"

[[notifications.push.sinks]]
name = "phone"
kind = "ntfy"
server = "https://ntfy.sh"
topic = "thegn-alerts"
min_priority = "notice"
```

Webhook, Discord, and Slack URLs are bearer credentials. They must be SecretRefs
using `env:VAR` or `file:PATH`; literal URLs are rejected by `thegn config
validate`. A route containing `push` fans out to every named sink, while
`push:oncall` selects only that sink. Each sink applies its own priority floor
after rules and do-not-disturb have been evaluated.

Delivery is best-effort and off-loop: queue overflow, provider rate limits, and
bounded retry failures are dropped or dead-lettered without affecting the
durable inbox row. Notification text can include branch names, issue titles,
and log fragments, so route chat sinks with care. `thegn doctor` performs an
offline configuration/request-shape probe and never posts a test message.

## Sound effects

The `[notifications.sound]` table controls the audible notification channel.
The default is `mode = "bell"`, which emits the terminal BEL; no audio file or
player is needed for a zero-configuration install. Custom effects are opt-in:
put a notification kind in `[notifications.sound.per_kind]` when that event
should use a file or trusted pack entry.

The available keys are:

- `mute` — globally disables sound while leaving the notification record and
  other channels intact.
- `mode` — `"bell"` (terminal BEL), `"chime"` (legacy generic file mode),
  `"command"` (legacy configured command mode), or `"off"`.
- `min_priority` — `"info"`, `"notice"`, or `"alert"`; sound below this
  priority is suppressed.
- `always_kinds` — notification kind names that bypass only `min_priority`.
  The defaults are `agent_done`, `agent_attention`, and `agent_failed`.
- `suppress_focused` — when true, suppresses sound for the worktree currently
  in view; it does not suppress the inbox or other eligible channels.
- `pack` — an absolute or `~`-expanded trusted directory containing sound
  files. It is scanned once at startup and on reload, not once per event.
- `volume` — a finite `0.0..=1.0` provider hint. Players without volume
  support use their default volume; `thegn doctor` reports that capability.
- `chime_file` — the legacy absolute or `~`-expanded file for `mode = "chime"`.
- `command` — the legacy user-configured shell command for `mode = "command"`.
- `per_priority` — legacy command-mode overrides keyed by `info`, `notice`, or
  `alert`.
- `per_kind` — a map from the single notification catalog's snake_case names
  to sound references.

`per_kind` values are deliberately not a command language. Accepted values
are `off`/`none`, `bell`/`terminal`, `builtin:bell`, `pack:<name>`, or an
absolute/`~`-expanded user file path. Bare pack names, relative paths, and
commands are rejected. For example:

```toml
[notifications.sound]
mode = "bell"
pack = "~/.local/share/thegn/sounds"

[notifications.sound.per_kind]
agent_attention = "pack:attention"
agent_done = "~/sounds/finished.wav"
queue_landed = "pack:merged"
test_failed = "off"
```

Pack and file references are trusted user configuration. Repository overlays
cannot introduce a pack, per-kind mapping, file, or command; configure those
globally or in the selected profile. A pack entry is resolved from the
startup/reload snapshot. Missing or unreadable references, missing providers,
and provider-unsupported formats fall back to the terminal BEL and produce a
best-effort diagnostic; they do not fail notification routing.

The pure sound policy applies gates before selecting a reference. The order is
global/route mute, DND and focused-worktree suppression, priority gates, a
matching rule's `sound`, `per_kind`, legacy `per_priority`, then the generic
`mode`. `always_kinds` bypasses only the minimum-priority gate. DND remains the
only quiet-hours schedule, and no sound control command or action is added.

The host selects an available provider using fixed argument vectors and plays
files on a bounded off-loop worker. Providers report supported formats and
whether they honor volume. Use `thegn doctor` to see the provider id and
availability, supported formats, volume capability, selected pack path, pack
entry count, and any fallback reason. Optional players and packs are
diagnostics, not doctor failures. Terminal-bell latches are coalesced; distinct
file jobs are queued best-effort and may be dropped if the bounded queue is
full.
