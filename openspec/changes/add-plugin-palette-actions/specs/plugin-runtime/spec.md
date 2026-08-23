## ADDED Requirements

### Requirement: Palette actions route to their owning plugin

Accepted `PaletteAction` contributions SHALL appear as command-palette rows keyed `plugin:<plugin>:<contribution>`, listed from loop-owned plugin state (never from config-only palette construction). Invoking a row SHALL send a resident plugin an `on_event` notification with `kind: Action` and the contribution id as `payload.id`, or run a one-shot plugin once off-loop; a disabled plugin's rows SHALL be absent and its invocation refused with a status message.

#### Scenario: A resident action fires an event

- **WHEN** the user picks a resident plugin's palette row
- **THEN** the plugin receives `on_event` with `kind: Action` and `payload.id` naming the contribution

#### Scenario: A one-shot action runs the plugin

- **WHEN** the user picks a one-shot plugin's palette row
- **THEN** the plugin's command runs once off-loop and its messages apply exactly like a scheduled run
