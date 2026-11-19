---
id: inbox
title: The inbox
parent: configuration
order: 34
actions: []
---

# The inbox

The native inbox brings your mail and tasks into thegn's notifications. The
daemon polls each account you configure and records new items as notification
rows of kind `inbox_item`, shown as `[<account>] <title>`. Rules can fire on
them (see [[automations]]) and the Lead reads them (see [[lead]]). It is
read-only: thegn never marks mail read, completes a task or replies.

It is off by default. Nothing runs unless `[daemon] enabled`,
`[daemon] headless_producers` and `[inbox] enabled` are all `true` and at least
one enabled account has an implemented kind. The UI never polls the inbox. No
rule is needed for items to appear: they are for you to see. Restart the daemon
after upgrading (schema v116 adds the `inbox` producer lease).

```toml
[daemon]
enabled = true
headless_producers = true

[inbox]
enabled = true
poll_interval_secs = 300   # default for accounts; never below 60
```

`[inbox]` is read from your global config only. It is not honoured in a repo,
workspace or profile overlay, and the Lead cannot set it.

## Accounts

Each `[[inbox.accounts]]` entry has a `name` (lowercase letters, digits, `_`
and `-`, up to 40 characters), a `kind`, and the common keys `enabled`,
`poll_interval_secs` (`0` inherits), `max_items` (1 to 500, default 50) and
`timeout_secs` (1 to 120, default 30).

### Gmail or any mail server over IMAP

Use `kind = "imap"` with an app password. Gmail needs 2-step verification on
and an app password; thegn has no Google sign-in.

```toml
[[inbox.accounts]]
name = "mail"
kind = "imap"
host = "imap.gmail.com"
port = 993                       # TLS
username = "you@example.com"
password = "keyring:inbox-mail"
mailbox = "INBOX"
unseen_only = true
```

Store the secret first, without it appearing on a command line:

```sh
printf %s "$APP_PASSWORD" | thegn secret set inbox-mail
```

### Todoist

```toml
[[inbox.accounts]]
name = "tasks"
kind = "todoist"
token = "keyring:inbox-todoist"
filter = "today | overdue"       # default
```

### Any command

`kind = "command"` runs an argv (no shell) and reads items as JSON from its
stdout. `exec` is accepted as an alias. This is how to read mail through
himalaya or issues through a Jira JQL script:

```toml
[[inbox.accounts]]
name = "himalaya"
kind = "command"
command = ["himalaya", "envelope", "list", "--output", "json"]

[[inbox.accounts]]
name = "jira"
kind = "command"
command = ["/home/me/bin/jira-inbox.sh"]   # runs your JQL, prints items
cwd = "/home/me"
env = { JIRA_TOKEN = "keyring:inbox-jira" }
```

A command's output is a JSON array of objects with `id` and `title`, and
optionally `url`, `from`, `labels`, `due`, `updated_at` and `priority`. An item
with a missing or invalid `id` is dropped.

### Reserved kinds

`gmail`, `jmap`, `outlook` and `slack` are reserved names. They are not
implemented, and an account using one is refused by `thegn config validate`.

## Secrets: credentials stay in the daemon

`token`, `password` and every `env` value are secret references
(`keyring:`, `env:` or `file:`). A literal `token` or `password` is refused by
`thegn config validate`. Set a keyring secret with `thegn secret set <name>`
(pipe the value in) and refer to it as `keyring:<name>`; see [[privacy]].

References are resolved in the daemon at fetch time, and only then. The Lead
never receives them: it sees only the sanitized notification rows and the
`producers.status` reply, whose error strings are scrubbed and capped at 200
characters.

## The first poll is a baseline

The first successful poll of an account, and the first after its settings
change, records what is there so old items do not fire. It shows at most the 20
newest as notifications and submits no rule events. Later polls notify, and
submit rule facts, for new ids only. An item that leaves the account's set and
comes back counts as new. The seen-set is kept per account (at most 2,000 ids)
in the database, separate from notifications, so clearing notifications never
re-fires old items.

## Untrusted text

Everything an item carries comes from outside. Before it is stored, titles,
senders, labels, URLs and ids are stripped of control, bidi and zero-width
characters, whitespace is collapsed, and lengths are capped (title 300, sender
120, URL 2048 and `http(s)` only or dropped, id 128 characters of
`A-Za-z0-9._:@+=-` or the item is dropped, up to 16 labels of 64 characters).
Item text never becomes a command, environment value, path, scope, rule,
autonomy level or approval.

## Commands

```sh
thegn inbox list [--account <a>] [--unread] [--json]
thegn inbox test <account> [--json]   # fetch once; shows counts, stores nothing
thegn inbox status [--json]           # per-account last poll, count, error
```

## Rules on inbox items

`when = "inbox_item"` rules can select with `inbox_account`, `inbox_kind` and
`inbox_label`. An item's title reaches a rule only as `message`.

- `inbox_item` rules are capped at `review` autonomy, never `merge`.
- A `pipeline.start`, `sessions.open` or `lead.wake` action on an `inbox_item`
  rule may not use `{message}` in its template; `thegn config validate`
  refuses it. A sender can write any title, so it must not reach a prompt.

## Why calendar is separate

Events are time-keyed and read by the bars and the calendar overlay with their
own sync, so they stay on the calendar seam (see [[calendar]]). The inbox is
for items that need attention, not a schedule.
