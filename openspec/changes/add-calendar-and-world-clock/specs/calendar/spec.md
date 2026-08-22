# Calendar

## ADDED Requirements

### Requirement: The date and clock widgets open a month calendar

Activating either the `date` or the `clock` masthead widget — by click, or by
`↵` with the bar focused — SHALL open a calendar popup anchored beneath it, and
a bindable action SHALL open the same popup from anywhere. The popup MUST show a
month grid, and MUST distinguish today from the selected day by different means
so both remain readable when they fall on the same cell. Days belonging to the
adjacent months MUST be visually subordinate to the focused month's.

The popup SHALL degrade rather than break: below the narrowest grid it MUST fall
back to a plain date/time readout, and it MUST render correctly on an
ASCII-only, non-colour terminal.

#### Scenario: Opening from the masthead

- **WHEN** a user clicks the date or clock widget
- **THEN** the calendar opens beneath that widget, showing the current month
  with today marked

#### Scenario: Opening from a keybinding

- **WHEN** a user presses the calendar chord
- **THEN** the calendar opens; pressing it again closes it

#### Scenario: Today and the selection coincide

- **WHEN** the selected day is today
- **THEN** both the today marking and the selection marking are still
  distinguishable

#### Scenario: A terminal too narrow for a grid

- **WHEN** the popup is opened on a terminal narrower than the tightest grid
- **THEN** a date, time and zone readout is shown instead of a broken grid

### Requirement: Month navigation is immediate and never blocks

The calendar SHALL support moving by day, week, month and year, jumping to
today, and jumping to the first or last day of the month, by keyboard and by
mouse. Because month geometry is computed locally, navigation MUST repaint
immediately for any month, whether or not that month's events are known — a
month with unknown events MUST show its grid at once and fill in event markers
when they arrive.

Paging by month from a day-of-month that the target month is too short to
contain MUST clamp for that month only, and MUST restore the original
day-of-month when paging on to a month long enough for it.

#### Scenario: Paging to a month whose events are not cached

- **WHEN** the user pages to a month with no cached events
- **THEN** the grid for that month renders immediately, and the event markers
  and agenda appear once the fetch completes

#### Scenario: Paging past a short month

- **WHEN** the user pages forward from the 31st into a 28-day month and then
  forward again
- **THEN** the selection is the 28th in the short month and the 31st in the next
  long one

#### Scenario: Mouse navigation

- **WHEN** the user clicks a day cell, a month chevron, or the today chip, or
  scrolls the wheel over the popup
- **THEN** the selection, month, or both change accordingly, and the popup stays
  open

### Requirement: World clocks are configurable and timezone-accurate

thegn SHALL show a configurable list of world clocks, always including the
user's own zone without configuration. Each clock MUST report its local time,
its daylight-aware zone abbreviation, its offset from the user's zone, and
whether it is on a different calendar date.

Offsets MUST be derived from the IANA database at the instant being displayed,
never stored, so that sub-hour offsets and periods when two zones disagree about
daylight saving are correct without special cases. The timezone database MUST be
available without relying on system-provided zone files.

A configured zone name that the database does not know MUST warn and omit that
one clock rather than failing startup, and configuration validation MUST report
it with a suggested correction.

#### Scenario: A zone on a sub-hour offset

- **WHEN** a clock is configured for a zone offset by a fraction of an hour
- **THEN** its offset from the user's zone is reported with minute precision

#### Scenario: Two zones mid-transition

- **WHEN** one zone has entered daylight saving and another has not
- **THEN** the reported difference between them reflects both zones' offsets at
  that moment

#### Scenario: An unknown zone name

- **WHEN** a clock names a zone the database does not contain
- **THEN** thegn warns, omits that clock, and continues; and configuration
  validation reports the name with a suggested correction

### Requirement: Events are read from configurable sources

thegn SHALL read calendar events from any number of configured accounts, each
naming a provider. It MUST support a local iCalendar file, a directory of them,
a subscribed iCalendar URL, a CalDAV collection, and an external program.

Sources are read-only: thegn MUST NOT modify a provider's data.

Each account MUST be independent — one account failing MUST NOT discard,
overwrite, or delay another account's events — and every event MUST carry the
identity of the account it came from, so events with the same provider-side
identifier from different accounts do not collide.

A refresh interval MUST be floored at a minimum regardless of configuration, so
a misconfigured value cannot poll a provider in a tight loop.

#### Scenario: A directory of iCalendar files

- **WHEN** an account points at a directory containing `.ics` files
- **THEN** every file in it is read as one calendar, and files that are not
  `.ics` are ignored

#### Scenario: One account is broken

- **WHEN** one configured account fails to fetch and another succeeds
- **THEN** the working account's events are stored and shown, and the failing
  account keeps its previously cached events

#### Scenario: A refresh interval of zero

- **WHEN** an account configures a refresh interval below the minimum
- **THEN** the minimum is used

### Requirement: Recurring events are expanded correctly across daylight saving

thegn SHALL expand recurring events from their recurrence rule, including
excluded and additional dates. Expansion MUST be performed in the event's own
timezone using wall-clock arithmetic, so a repeating event keeps its local time
across a daylight-saving transition rather than shifting.

A recurrence that names a day a given month does not contain MUST skip that
month rather than moving the occurrence to a nearby day. An occurrence landing
in a nonexistent local time MUST be shifted by the transition's width, keeping
distinct occurrences distinct; one landing in a repeated local time MUST resolve
to the earlier instant. An excluded occurrence MUST still count against a rule's
occurrence limit.

Expansion MUST be bounded by the requested window, so a rule with no end costs
no more than one with an end.

#### Scenario: A weekly meeting across a transition

- **WHEN** a weekly event in a zone that observes daylight saving is expanded
  across the transition
- **THEN** every occurrence keeps the same local time, and their absolute
  instants differ by the transition

#### Scenario: A monthly rule on a day some months lack

- **WHEN** a monthly recurrence falls on a day-of-month that a given month does
  not have
- **THEN** no occurrence is produced for that month

#### Scenario: A rule with no end date

- **WHEN** an unbounded recurrence is expanded over a one-month window
- **THEN** only that month's occurrences are produced and expansion terminates

### Requirement: Reminders are raised through the notification system

When reminders are enabled, thegn SHALL raise a notification ahead of an event
that carries a reminder, using a configurable default lead time for events whose
source supplies none. Reminders MUST be delivered as ordinary notifications, so
existing notification routing, priority, quiet-hours and desktop settings apply
without calendar-specific configuration.

A reminder MUST be raised once and not repeat, including across a restart. A
cancelled event MUST NOT raise one. A large jump in the system clock MUST NOT
cause a backlog of reminders to be raised at once.

The due check MUST NOT require its own timer and MUST NOT run at all when no
account is configured or reminders are disabled.

#### Scenario: A reminder fires once

- **WHEN** an event's reminder lead time is reached
- **THEN** exactly one notification is raised, and subsequent checks before the
  event starts raise no further ones

#### Scenario: Restarting after a reminder fired

- **WHEN** thegn is restarted and the due check runs again for the same
  occurrence
- **THEN** no duplicate notification is raised

#### Scenario: Reminders are disabled

- **WHEN** reminders are turned off, or no account is configured
- **THEN** no due check is scheduled and no idle wake is incurred for it

### Requirement: The clock is accurate to its displayed resolution

The masthead clock SHALL update when the displayed text would change — on the
minute boundary for a minute-resolution format, and each second only when the
configured format renders seconds. Updating MUST repaint only the bars, never
the full chrome, and MUST NOT introduce a polling timeout on the idle path.

Both bar format strings MUST be validated when configuration loads. An invalid
format MUST warn and fall back to the default for that field, because the
formatting failure would otherwise surface as a crash during rendering.

#### Scenario: Crossing a minute boundary

- **WHEN** the wall clock crosses a minute
- **THEN** the clock repaints, and only the bar regions are recomposed

#### Scenario: An invalid format string

- **WHEN** a bar format string contains an invalid specifier
- **THEN** loading warns and uses the default for that field, and rendering
  proceeds normally

#### Scenario: The popup is open across midnight

- **WHEN** the calendar is left open past midnight
- **THEN** the highlighted day advances to the new date
