# Notifications

## ADDED Requirements

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

Qualifying notifications SHALL emit an audible cue per `[notifications.sound]`:
`mode = "bell"` (default) writes a terminal `BEL`, `mode = "command"` runs a
configured command, and `mode = "off"` is silent. Sound MUST only fire for
notifications at or above `[notifications.sound] min_priority` and MUST be
best-effort and off the event loop's critical path (the terminal `BEL` is
written on the next render flush; a command spawns off-thread).

#### Scenario: Bell on alert

- **WHEN** an Alert notification arrives with `mode = "bell"` and
  `min_priority = "alert"`
- **THEN** a terminal BEL is emitted on the next render flush

#### Scenario: Below threshold is silent

- **WHEN** a Notice notification arrives with `min_priority = "alert"`
- **THEN** no sound is emitted
