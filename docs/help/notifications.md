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
