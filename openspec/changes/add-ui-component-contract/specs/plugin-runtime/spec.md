# plugin-runtime — delta for add-ui-component-contract

## ADDED Requirements

### Requirement: Plugin elements render through the shared element contract

Accepted plugin contributions with a rendering surface (`StatusBarSegment`, and `PanelSection` once negotiated) SHALL render through the shared element contract: the plugin's cached view becomes element rows built by the host, emitting hit spans from the same build that paints, placed by the plugin's `plugin:<plugin>:<contribution>` id under the config placement grammar. The existing `SurfaceCache` budget/degrade machinery SHALL bound plugin render cost on every surface — multi-row views are truncated host-side to the surface's row budget, and a budget-exceeded or crashed plugin degrades its element rather than stalling composition. Plugin view content is untrusted display data: text becomes cell content and styling resolves through the host's own token vocabulary — no plugin-supplied bytes reach the terminal as control sequences, and no plugin content can name or dispatch a host action.

#### Scenario: A pathological view degrades, never stalls

- **WHEN** a plugin sends an `update` whose view exceeds the surface's render budget
- **THEN** the surface serves the degraded view and composition proceeds, exactly as statusbar segments degrade today

#### Scenario: A million-row update is bounded

- **WHEN** a plugin's panel-section view carries vastly more rows than the section's budget
- **THEN** the host truncates to the budget at build time and the frame cost matches a long native list

#### Scenario: Element updates ride the existing wake path

- **WHEN** a resident plugin updates a placed element while the loop is blocked
- **THEN** the reader thread's channel send and waker pulse deliver it, and the next frame recomposes chrome — no new wake path and no polling
