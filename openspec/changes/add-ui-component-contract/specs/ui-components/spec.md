# ui-components — delta for add-ui-component-contract

## ADDED Requirements

### Requirement: A chrome element is declared through one contract

A chrome element SHALL be declared as data through one shared contract: a stable element id, its zone (the same zone vocabulary the help system's `zone:*` / `panel:*` context keys use), its content as rows of the `Line`/`Tok` segment model (generalizing `PanelRow`: line + optional row background + optional hit), its hit spans, its zone-local key table, and its placement id. Native and plugin elements MUST use the same contract; a plugin element's id SHALL be `plugin:<plugin>:<contribution>` in the same namespace native ids occupy. The contract is declarative data consumed by the existing compositor — it SHALL NOT introduce a per-element paint callback, a layout engine, or any change to the pure render decision: an element change composes as chrome (a `Full` frame) exactly as chrome changes do today.

#### Scenario: A native badge is declared once

- **WHEN** a statusbar badge is expressed as an element
- **THEN** its rows, hit spans, keys and placement id come from one declaration, and no separate hit table, hint list, or draw routine exists for it

#### Scenario: Elements do not perturb the render decision

- **WHEN** pane output arrives and no element changed
- **THEN** `render_plan::plan` still returns `Panes` and no element is recomposed

### Requirement: Hit spans are emitted by the build that paints

Every element's hit targets SHALL share one span signature — a screen rectangle paired with an element action — and SHALL be emitted by the same build pass that produced the painted rows, so a hit table can never be derived from a different list than the painter used. Mouse resolution for element zones SHALL go through one shared lookup over these spans rather than per-zone geometry re-derivation. Interactive elements without hit spans SHALL NOT be added; existing click-dead chrome (pin chips) MUST gain hit spans when migrated.

#### Scenario: A shed badge cannot be clicked by ghost geometry

- **WHEN** the statusbar fitter sheds a badge under width pressure
- **THEN** the badge's hit span is absent from the same build output, and a click at its former cells resolves to whatever is actually painted there

#### Scenario: Pin chips become clickable

- **WHEN** the pin strip is migrated to the element contract
- **THEN** each chip emits a hit span from its painting build and a click activates that pin

### Requirement: Element migration is ratcheted

Legacy chrome draw sites that compose outside the element contract (raw `draw_text` with manual x math, hit tables not emitted by their painter) SHALL be pinned in a shrink-only allowlist (`test/element-ratchet.txt`) checked by the lint gate, with the same mechanics as the existing architecture ratchets: a new ad-hoc draw site fails the gate naming the contract, migrating a site deletes its line, and the file only shrinks. Overlays and the sidebar renderer migrate opportunistically under the ratchet rather than being rewritten in one change.

#### Scenario: A new ad-hoc draw site is refused

- **WHEN** a new chrome zone paints interactive content with raw draw calls and a hand-built hit table, without an allowlist entry
- **THEN** the ratchet check fails, naming the file and pointing at the element contract

#### Scenario: Migration shrinks the ratchet

- **WHEN** `draw_center_tabs` is migrated to the element contract
- **THEN** its `test/element-ratchet.txt` line is deleted and the gate passes with the smaller file

### Requirement: Placement and visibility are config, in the bars grammar

Element placement SHALL be driven by the existing `[bars]` grammar — ordered id lists per slot, omission hides, an unknown id warns and is skipped (never a hard error) — extended to statusbar badges and plugin elements: badges get placement ids usable in the `[bars]` lists, and plugin elements are placeable by their `plugin:<plugin>:<contribution>` id in the same lists. Badge ids absent from config SHALL keep today's default order appended after the listed items, so existing configs render identically. Placement SHALL control order and visibility only — it grants no capability, and the statusbar's priority shedding still applies after placement. Every new placeable id MUST be documented in `config/config.toml.example`.

#### Scenario: A badge is reordered by config

- **WHEN** `bottom_right` lists a badge id before the `status` widget
- **THEN** the badge renders in that position instead of the hardcoded source order

#### Scenario: A stale plugin id degrades softly

- **WHEN** a `[bars]` list names a plugin element whose plugin is disabled or gone
- **THEN** the id is skipped with a warning and the rest of the list renders
