# notifications Specification

## Purpose

The aggregated notification and event bus: user-defined routing rules, DND / quiet-hours, per-profile routing, sound/bell tiers, desktop notifications, and the in-app inbox.

## Requirements

### Requirement: User-defined action rules route notifications

Notification delivery SHALL be governed by an ordered list of user rules
(`[[notifications.rules]]`) evaluated at dispatch time. A rule MATCHES a
notification when every present selector (kind, worktree glob, source prefix,
message regex, minimum priority, active mode, profile) matches, and its action
MAY override the effective priority, restrict the delivery channels, mute the
ephemeral channels, drop the notification entirely, or set a sound. Rules with
no matching selector are wildcards; evaluation MUST stop after a rule marked
`stop`.

#### Scenario: Mute by worktree

- **WHEN** a rule matches a worktree glob with `mute = true`
- **THEN** notifications from that worktree record in the inbox but raise no
  desktop toast, in-app toast, or sound

#### Scenario: Message regex drops noise

- **WHEN** a rule matches a `message` regex with `drop = true`
- **THEN** the matching notification is neither recorded nor delivered

#### Scenario: Rule promotes priority

- **WHEN** a rule sets `set_priority = "alert"` for a normally-Notice kind
- **THEN** that notification's effective priority becomes Alert, so it qualifies
  for the sound cue and breaks through do-not-disturb

### Requirement: Do-not-disturb suppresses low-priority delivery

A do-not-disturb state SHALL suppress desktop toasts, in-app toasts, and sound
for notifications below `[notifications.dnd] allow_priority`, while still
recording them in the inbox. DND MUST be active either during a configured quiet
window (`[notifications.dnd] windows`, supporting wrap-past-midnight ranges and
weekday tokens) or when the runtime toggle forces it on; the runtime toggle MUST
override the schedule.

#### Scenario: Quiet hours hold a normal notification

- **WHEN** the current time is inside a configured quiet window and a
  below-`allow_priority` notification arrives
- **THEN** it is recorded in the inbox but no toast, desktop, or sound fires

#### Scenario: Alert breaks through DND

- **WHEN** DND is active and an Alert-priority notification arrives with
  `allow_priority = "alert"`
- **THEN** it delivers on all its routed channels

#### Scenario: Manual toggle overrides schedule

- **WHEN** the runtime DND toggle is forced off during a quiet window
- **THEN** notifications deliver normally regardless of the schedule

### Requirement: Routing modes and per-profile overlays

The active routing mode SHALL scope which rules apply via each rule's `modes`
selector, and per-profile notification settings (`[profiles.<p>.notifications]`)
MUST layer onto the global `[notifications]` config for the active profile,
following the same precedence as keybind and sandbox profile overlays. The
active mode (`[notifications] active_mode`) is switchable at runtime.

#### Scenario: Mode gates a rule

- **WHEN** a rule lists `modes = ["focus"]` and the active mode is not `focus`
- **THEN** the rule does not apply

#### Scenario: Profile overlay layers

- **WHEN** a profile is active with a `[profiles.<p>.notifications]` overlay
- **THEN** its settings override the global notification config for that profile

### Requirement: Sound and bell channel

Qualifying notifications SHALL resolve an audible result through
`[notifications.sound]` after the existing mute, route, DND,
focused-worktree, and priority gates. `mode = "bell"` is the default and
emits a terminal `BEL`. `mode = "chime"` is a legacy generic file mode: it
plays `chime_file` when configured and otherwise emits the terminal bell.
`mode = "command"` runs the trusted configured command off-loop, and
`mode = "off"` is silent. `always_kinds` SHALL bypass only `min_priority`;
they SHALL NOT bypass mute, DND, a rule route that excludes sound, or focused
worktree suppression. `volume` SHALL be validated as a finite value in
`0.0..=1.0` and passed as a best-effort provider hint.

File playback, command execution, provider probing, and filesystem access
MUST stay off the event loop. Missing or unreadable references, packs,
providers, unsupported formats, and playback failures SHALL produce a
best-effort diagnostic and fall back to the terminal bell where an audio file
was requested. A provider without volume support SHALL use its default
invocation and doctor SHALL report that limitation.

#### Scenario: Bell on alert

- **WHEN** an Alert notification qualifies with the default sound config
- **THEN** a terminal BEL is emitted through the existing coalesced latch
  without requiring an audio provider or file

#### Scenario: Below threshold is silent

- **WHEN** a Notice notification is evaluated with `min_priority = "alert"`
  and is not in `always_kinds`
- **THEN** no sound is emitted

#### Scenario: Chime without a file uses the bell

- **WHEN** `mode = "chime"` has no `chime_file`
- **THEN** the terminal bell is emitted and no synthesized or bundled file is
  created

#### Scenario: File playback falls back to the bell

- **WHEN** an eligible file reference has no provider, has an unsupported
  format, or cannot be resolved
- **THEN** a best-effort diagnostic is recorded and the terminal bell latch is
  requested

#### Scenario: Volume is best-effort

- **WHEN** `volume = 0.3` and the selected provider supports volume
- **THEN** the provider receives the hint; with a provider that does not
  support volume, playback uses its default and doctor reports unsupported

### Requirement: Notification priority is derived from kind with overrides

Each notification's priority (Alert / Notice / Info) SHALL be derived from its `NotificationKind` at read time (no stored priority column) and MUST be overridable per kind via `[notifications.priority]`, so a config remap reclassifies even historical rows live.

#### Scenario: Default classification

- **WHEN** priorities are computed with no config override
- **THEN** the four failure kinds are Alert, lifecycle kinds (worktree created,
  process exited) are Info, and the rest are Notice

#### Scenario: Config remap reclassifies live

- **WHEN** `[notifications.priority]` demotes a kind
- **THEN** existing rows of that kind are reclassified without a migration

### Requirement: Priority coherently drives flag, count, and toast

The red attention flag SHALL count only unread Alert notifications, the neutral unread count SHALL be unread Alert+Notice, Info MUST never increment any counter (but still appears in the inbox), and toast urgency MUST follow priority (Alert→Critical, Notice→Normal, Info→Low). Priority is the _effective_ priority (`[notifications.priority]` overrides applied) everywhere it is read — badge counts, the statusbar chip, and the unified surface's grouping MUST agree.

#### Scenario: Info-only inbox raises no flag

- **WHEN** the only unread notifications are Info (e.g. worktree created)
- **THEN** the panel header shows a neutral state with no red attention flag

#### Scenario: A failure raises the flag

- **WHEN** a TestFailed notification is unread
- **THEN** the header shows the red attention flag and a Critical desktop toast is
  eligible

### Requirement: One statusbar attention chip

The statusbar SHALL carry a single attention chip: `✋ N` where N is the number of needs-you worktrees plus unread Alert notifications not attributed to one of those worktrees (red when anything is blocked/failing or an alert is unread, amber otherwise); when N is zero and unread Notice rows exist, a quiet `✉ N` inbox count instead; nothing when only Info rows are unread. The chip's count MUST equal the number of rows in the unified surface's "Needs you" and "Alerts" groups.

#### Scenario: A failed pane lights one chip, not two

- **WHEN** a `process_failed` notification for worktree W is unread and W scores as a needs-you worktree
- **THEN** the statusbar shows `✋ 1` (not a separate `⚑ 1`) and the unified surface lists W once under Needs you

### Requirement: The unified surface shows live items and acts in place

The unified surface SHALL list only unread notifications (read rows are history, shown by the panel inbox's show-read toggle), SHALL be sized to the terminal (growing with content up to ¾ of the screen width and the drawable height), and navigating its cursor MUST NOT mark anything read. `x` SHALL dismiss/quiet the selected row and remove it in place with the popup open; `a` SHALL clear all and close.

#### Scenario: Dismissing a row is visible where it was pressed

- **WHEN** the user presses `x` on an unread notification row
- **THEN** the row (and an emptied group header) leaves the list, the popup stays open, and the chip count drops on the next refresh

### Requirement: "Worktree ready" notifications are opt-in per env

The `worktree_created` notification SHALL be recorded and routed only when the worktree's env sets `[env.<name>] notify_ready = true`; by default no env notifies. The status line SHALL report readiness regardless, and the lifecycle event SHALL still reach the event bus.

#### Scenario: Default env creates silently

- **WHEN** a worktree finishes bring-up on an env without `notify_ready`
- **THEN** no inbox row, toast, or sound is produced; the status line reads "worktree <branch> ready"

#### Scenario: Opted-in env notifies

- **WHEN** `[env.sprites] notify_ready = true` and a worktree on `sprites` finishes bring-up
- **THEN** a `worktree_created` row is recorded and routed per `[notifications]` rules

### Requirement: The kind list has one source

`NotificationKind::ALL` SHALL be the only list of built-in notification kinds: `thegn notify push --help` MUST generate its kind list (with default priorities) from it, and `config.toml.example`'s `[notifications.priority]` prose MUST name every kind, pinned by a test.

#### Scenario: New kind

- **WHEN** a kind is added to the enum but not to the example prose
- **THEN** `example_config_prose_names_every_kind` fails naming it

### Requirement: Named push sinks share notification routing

The notification configuration SHALL support ordered, uniquely named push
sinks. `push` SHALL select every configured sink, `push:<name>` SHALL select
only that sink, and an optional per-sink minimum priority SHALL be applied to
the effective routed priority. The legacy scalar ntfy configuration SHALL
continue to behave as one sink.

#### Scenario: A rule targets one sink

- **WHEN** a rule routes a matching notification to `push:oncall`
- **THEN** only the configured `oncall` sink receives it

#### Scenario: Sink floors apply independently

- **WHEN** a Notice routes to all sinks and a Slack sink requires Alert
- **THEN** eligible lower-floor sinks receive it and the Slack sink does not

### Requirement: Webhook, Discord, and Slack are implemented sink kinds

The push-provider seam SHALL implement `webhook`, `discord`, and `slack`
beside `ntfy`. Generic webhook payloads SHALL use the stable versioned fields
`v`, `kind`, `priority`, `message`, `source`, `worktree`, and `ts`. Discord and
Slack payloads SHALL obey their text bounds and present effective priority;
Discord payloads MUST suppress automatic mention parsing. `telegram`,
`gotify`, and `pushover` SHALL remain reserved until separately implemented.

#### Scenario: A queue alert reaches Slack

- **WHEN** an Alert-priority queue notification routes to a Slack sink
- **THEN** the provider sends a bounded Slack webhook payload with the alert
  presentation while preserving the ordinary inbox record

#### Scenario: Discord text cannot mass-mention

- **WHEN** routed notification text contains `@everyone`
- **THEN** the Discord payload disables mention parsing

### Requirement: Webhook endpoints remain secret

Discord, Slack, and generic webhook endpoints SHALL be configured only through
`env:` or `file:` SecretRefs. Raw URLs MUST fail config validation, and
resolved endpoints MUST NOT appear in logs, error text, or probe output.

#### Scenario: A raw URL is rejected

- **WHEN** a sink declares `url = "https://hooks.example/secret"`
- **THEN** validation names the sink and requires a SecretRef without echoing
  the URL

### Requirement: Sink delivery is bounded and off-loop

Each configured sink SHALL deliver from its own bounded worker with a
provider-appropriate token bucket, bounded transient retries, and bounded
`Retry-After` handling. Queue overflow or a rate-limit wait beyond the retry
budget SHALL drop and count the message rather than block producers or grow an
unbounded backlog.

#### Scenario: A burst cannot wedge notification producers

- **WHEN** a sink receives more messages than its queue/rate budget permits
- **THEN** excess messages are counted as dropped and producer execution
  continues without waiting for network I/O

### Requirement: Provider probes do not send messages

Sink probes SHALL report provider capability, configuration/secret-resolution
state, and offline request-shape readiness without making a network POST.

#### Scenario: Diagnostics are channel-silent

- **WHEN** a configured Discord or Slack provider is probed
- **THEN** no visible test message is delivered and no endpoint is revealed

### Requirement: Push-to-phone is a routed delivery channel behind a provider seam

The notification router SHALL support a `push` delivery channel governed by
the same machinery as every existing channel: rules `channels` selectors MAY
include or exclude `push`, DND SHALL suppress it below `allow_priority` (it is
an ephemeral channel; the inbox row remains the durable record), and mode/
profile overlays and burst debouncing apply unchanged. Delivery SHALL go
through a push-provider seam (object-safe trait, config `kind`
implemented-or-`reserved`, `Probe` in `thegn doctor`) whose first implemented
kind is `ntfy` — POSTing to a configured `server`/`topic` with the effective
priority mapped (Alert→high, Notice→default, Info→low) — with `telegram`,
`gotify`, `pushover`, and `webhook` reserved. Publishing MUST be best-effort
and off the event loop (bounded worker, bounded retries, drop-on-overflow;
a push failure never blocks or fails the notification), and the auth token
MUST be a SecretRef (`env:`/`file:`) — never a raw token in config.

#### Scenario: An alert reaches the phone

- **WHEN** `[notifications.push]` is configured with kind `ntfy` and an
  Alert-priority notification passes the rules with `push` among its channels
- **THEN** the ntfy topic receives a high-priority message carrying the
  notification's title and message, published off-loop, and the inbox row is
  recorded exactly as before

#### Scenario: DND holds push like any ephemeral channel

- **WHEN** DND is active with `allow_priority = "alert"` and a Notice-priority
  notification arrives
- **THEN** no push is published; the notification records in the inbox

#### Scenario: An unreachable push server costs nothing

- **WHEN** the configured ntfy server is unreachable
- **THEN** the publish retries a bounded number of times off-loop and is then
  dropped with a counter — the event loop never blocks and no notification is
  lost from the inbox

#### Scenario: Doctor probes the push seam

- **WHEN** `thegn doctor` runs with a push channel configured
- **THEN** a `push`/`ntfy` probe row reports availability (server
  reachability, token presence) in the standard probe shape

### Requirement: Clearing the inbox clears exactly what the inbox displays

The repo-scoped inbox's "clear all" SHALL mark read exactly the set the
repo-scoped inbox displays, evaluated by one shared predicate — untagged
(host-global) rows, rows tagged with one of the repo's registered worktrees, and
rows tagged with a worktree path the registry does not know (the repo's main
checkout, an externally-created worktree). It MUST NOT mark read a row tagged
with a known worktree of a different repo. The all-worktrees view SHALL clear
every row regardless of tag. Clearing MUST also lower the live raised hands for
the same scope, so a quieted worktree does not have its demand returned by the
next hydration.

#### Scenario: A row tagged to the repo's main checkout is displayed and cleared

- **WHEN** a notification is tagged with the repo's own main checkout path,
  which never gets a worktree-registry row, and the user clears all in the
  repo-scoped inbox
- **THEN** the row is both displayed by the inbox and marked read, and it stays
  read across a rehydrate

#### Scenario: Another repo's known worktree is neither displayed nor cleared

- **WHEN** a notification is tagged with a registered worktree belonging to a
  different repo and the user clears all in the repo-scoped inbox
- **THEN** the row is not displayed and is left unread

#### Scenario: An untagged row is displayed and cleared

- **WHEN** a host-global notification carrying no worktree tag is in the inbox
  and the user clears all in the repo-scoped inbox
- **THEN** the row is displayed and marked read

#### Scenario: The all-worktrees view clears everything

- **WHEN** the inbox has been widened to every worktree and the user clears all
- **THEN** every unread notification is marked read regardless of its worktree
  tag, and every live raised hand is lowered

#### Scenario: Clearing lowers the live raised hands

- **WHEN** a worktree with a live raised hand is acknowledged, or the inbox's
  clear-all runs over a scope containing it
- **THEN** the live per-session attention state for that worktree is deleted and
  the worktree does not return to the needs-you state on the next hydration

### Requirement: Per-event sound mapping

`[notifications.sound.per_kind]` SHALL map the snake_case names from
`NotificationKind::ALL` to `SoundRef` values. The map SHALL be resolved after
all audible gates and SHALL override legacy per-priority command-mode values
and the generic mode. A matched rule's `sound` action SHALL override the
per-kind map. `off`/`none` SHALL silence the kind; bell aliases and
`builtin:bell` SHALL select the terminal bell; `pack:<name>` SHALL select a
trusted pack entry; and an absolute or `~`-expanded path SHALL select a user
file. Bare pack names, relative paths, and commands SHALL be rejected for
per-kind values. Unknown kind names and malformed references SHALL be
reported by core validation with a did-you-mean suggestion when applicable.

#### Scenario: Two kinds, two sounds

- **WHEN** `per_kind` maps `agent_attention` and `test_failed` to different
  valid file references and both notifications qualify
- **THEN** each file job is enqueued independently on the bounded off-loop
  worker, subject to best-effort queue capacity

#### Scenario: Rule still wins

- **WHEN** a matched rule sets `sound = "off"` for a kind that has a
  per-kind file
- **THEN** no sound job is enqueued

#### Scenario: Unknown kind name

- **WHEN** `per_kind` contains a misspelled catalog kind
- **THEN** core validation reports the unknown name and suggests the nearest
  known kind; it does not become a new runtime event kind

### Requirement: Trusted sound packs

`[notifications.sound].pack` SHALL be an absolute or `~`-expanded trusted
directory. A `pack:<name>` reference SHALL resolve from the immutable snapshot
built at startup and configuration reload, without per-event filesystem
access. The host MAY index both a pack filename and its stem; the selected
provider SHALL decide whether the file extension is supported. Missing,
unreadable, or unsupported pack entries SHALL fall back to the terminal bell.
There SHALL be no manifest requirement, filename-derived default sound,
synthesized sound family, or recorded audio asset shipped by this change.

#### Scenario: Pack entry resolves by explicit name

- **WHEN** `pack` points to a readable directory containing `attention.wav`
  and `per_kind.agent_attention = "pack:attention"` qualifies
- **THEN** the startup/reload snapshot resolves that entry and the provider
  receives the file off-loop

#### Scenario: Missing pack degrades

- **WHEN** `pack` points to a directory that does not exist
- **THEN** doctor reports the fallback reason and an eligible pack reference
  requests the terminal bell without blocking notification routing

### Requirement: Live attention sound edge

The live `session_attention` state SHALL be observed during hydration, not
render. The first snapshot SHALL seed `(session, since)` values without sound.
Thereafter, a new session or changed `since` SHALL route one synthetic
`agent_attention` cue through `NotifyState`; cleared or removed sessions SHALL
be forgotten. This observer SHALL NOT insert a duplicate durable inbox row.

#### Scenario: Baseline and changed hand

- **WHEN** hydration sees an existing session hand for the first time and later
  sees the same session with a changed `since` value
- **THEN** the baseline emits no cue and the changed hand routes exactly one
  transient `agent_attention` cue without inserting an inbox row

### Requirement: Sound diagnostics and boundaries

`thegn doctor` SHALL report the host sound provider id and availability,
supported formats, volume capability, selected pack path, pack entry count,
and fallback reason in its existing text and JSON provider surfaces. Missing
optional players, packs, and files SHALL be diagnostics rather than doctor
failures. THE-35 SHALL add no CLI action, control snapshot field, MCP tool,
completion slot, capability-catalog row, or SQLite state.

#### Scenario: Optional sound remains diagnostic

- **WHEN** no supported player or configured pack is available
- **THEN** doctor reports the unavailable provider or pack fallback while the
  command remains successful and no control, capability, or database surface is
  added
