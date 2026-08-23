## ADDED Requirements

### Requirement: A plugin can be an issue provider

A resident plugin with an accepted `IssueProvider` contribution SHALL be bridged onto the issue seam: each `IssueBackend` operation is sent as a `provider.call` request (`{"seam":"issues","op":…,"args":…}`) and the plugin's `RpcResponse` is the operation's result, with `unsupported` errors mapping to the seam's optional-op fall-through and unanswered calls timing out at the plugin's `timeout_secs`. Live plugin providers SHALL join every `IssueRouter` the host builds, labeled by the contribution's label, and leave it on exit/disable. `CiProvider` and `ForgeProvider` SHALL be accepted wire vocabulary negotiated unsupported until their seams support dynamic selection.

#### Scenario: Plugin issues join the panel feed

- **WHEN** a resident plugin with an `IssueProvider` contribution answers `list_issues`
- **THEN** its issues merge into the router's results beside configured accounts, with provider slug `plugin:<id>`

#### Scenario: A silent plugin degrades, never hangs

- **WHEN** a bridged operation gets no reply within the plugin's timeout
- **THEN** the call returns a classified transport error and a late reply is dropped
