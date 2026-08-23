## 1. Wire + bridge

- [x] 1.1 `ExtensionPoint::{IssueProvider, CiProvider, ForgeProvider}` + `surface:provider` + `PROVIDER_CALL_METHOD`; plugin-api snapshot regenerated (additive)
- [x] 1.2 `ProviderBridge` (correlation, timeout, resolve) + `PluginIssueBackend` (full IssueBackend); scripted end-to-end tests (round-trip, unsupported, timeout, dead session, unroutable reply)

## 2. Host integration

- [x] 2.1 Bridge lifecycle on PluginEntry (build/exit/respawn/disable) + `SessionEvent::Response` routing
- [x] 2.2 `plugin_providers` registry + `IssueRouter::push_backend` + hydration sites append live providers
- [x] 2.3 Contract accepts IssueProvider only (Ci/Forge documented unsupported); help + extending docs

## 3. Gate

- [x] 3.1 clippy + plugin/issue suites + `just lint`; openspec validate
