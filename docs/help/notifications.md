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
