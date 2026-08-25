# Plugin Runtime — tracker capability delta

## MODIFIED Requirements

### Requirement: A plugin can be an issue provider

A resident plugin with an accepted `IssueProvider` contribution SHALL be
bridged onto the issue seam: each `IssueBackend` operation is sent as a
`provider.call` request (`{"seam":"issues","op":…,"args":…}`) and the plugin's
`RpcResponse` is the operation's result, with `unsupported` errors mapping to
the seam's optional-op fall-through and unanswered calls timing out at the
plugin's `timeout_secs`. Live plugin providers SHALL join every `IssueRouter`
the host builds, labeled by the contribution's label, and leave it on
exit/disable. `CiProvider` and `ForgeProvider` SHALL be accepted wire
vocabulary negotiated unsupported until their seams support dynamic selection.

The `IssueProvider` contribution SHALL accept an optional `caps` declaration
(the same field names as `TrackerCaps`; omitted means all false), declared
statically in the manifest — never fetched by a per-build RPC — and the
bridged backend's `caps()` MUST return it, gating the chrome for plugin
providers exactly as for native ones. Operations whose declared capability is
false MUST be refused locally with a typed `Unsupported` error, without a
round-trip to the plugin; operations whose capability is true ride
`provider.call`, with the existing `unsupported`-reply fall-through retained
as a second net so an overclaiming plugin degrades instead of erroring the
panel. The op vocabulary SHALL extend additively with the tracker-tier
operations (`available_transitions`, `transition`, `list_projects`,
`project_items`, `list_cycles`) on the same wire; plugins without declared
caps never receive the new ops. Bridged providers MUST satisfy the same
offline caps⇔ops conformance contract as native providers.

#### Scenario: Plugin issues join the panel feed

- **WHEN** a resident plugin with an `IssueProvider` contribution answers `list_issues`
- **THEN** its issues merge into the router's results beside configured accounts, with provider slug `plugin:<id>`

#### Scenario: False-cap op is refused without a round-trip

- **WHEN** a plugin's manifest omits `caps` (all false) and the panel path invokes `add_comment` on one of its items
- **THEN** the bridge returns a typed `Unsupported` error locally and no `provider.call` request is sent to the plugin

#### Scenario: Declared caps gate the chrome like a native provider

- **WHEN** a plugin declares `caps = { comments = true }` in its `IssueProvider` contribution
- **THEN** the panel offers the comment action on its items, the call rides `provider.call` with op `add_comment`, and tier actions remain hidden

#### Scenario: Old plugins are untouched by the vocabulary extension

- **WHEN** a plugin written before the tracker-tier ops registers with no `caps` field
- **THEN** its five core ops behave exactly as before and none of the new tier ops are ever sent to it
