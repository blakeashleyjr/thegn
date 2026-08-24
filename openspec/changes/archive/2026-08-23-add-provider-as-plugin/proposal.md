## Why

Every seam converged on object-safe traits precisely so "a new provider is an implementation, never a rewrite" — but implementations still had to be compiled in. The plugin runtime made plugins first-class processes; the last step of the extensibility programme (audit row C4) is letting a plugin _be_ a provider, starting with the seam whose selection is naturally open (issue accounts are a list, not a closed kind enum).

## What Changes

- Wire (additive, snapshot regenerated): `ExtensionPoint::{IssueProvider, CiProvider, ForgeProvider}` (`surface:provider` capability) and the `provider.call` host→plugin request method (`{"seam","op","args"}` params, answered by `RpcResponse`; `unsupported` maps to the seam's optional-op fall-through).
- `thegn_svc::plugin::ProviderBridge`: generic correlation over a resident session — id allocation, pending map, per-plugin timeout, `resolve` fed by the host's drain; a dead or silent plugin degrades to a classified transport error, never a hang.
- `PluginIssueBackend`: the full `IssueBackend` implemented over the bridge (all eight ops, `plugin:<id>` provider slug), tested end-to-end against a scripted shell plugin.
- Host: resident plugins with an accepted `IssueProvider` contribution get a bridge; `SessionEvent::Response` routes to it; a process-global registry (`plugin_providers`, the `ci_refresh` health-map idiom) publishes live bridges on load/exit/respawn/disable, and every hydration site's `IssueRouter` appends them (`IssueRouter::push_backend`) — plugin issues appear in the panel as extra accounts.
- Host contract accepts `IssueProvider`; `CiProvider`/`ForgeProvider` stay wire vocabulary negotiated unsupported — their seams select by closed `config_enum!` kinds, so honest acceptance needs a dynamic-selection story first (documented in the contract).

## Impact

- Audit row C4. Specs: plugin-runtime + provider-seams deltas; plugin-api snapshot regenerated (additive).
- Code: thegn-core (plugin_api wire, issue model serde), thegn-svc (plugin/provider.rs, IssueRouter), thegn-host (bridge lifecycle, plugin_providers registry, hydration).
