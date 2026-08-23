## Why

The plugin runtime (add-plugin-runtime) rendered statusbar segments and notifications, but a plugin could not _do_ anything on demand: the `PaletteAction` extension point existed in the wire contract with no host behind it, so plugins were passive data sources. (The audit row C2 also asked for the calendar `command` backend to share the plugin process machinery — that happened by construction when `spawn_ndjson` became `thegn_svc::plugin::proc`; the calendar source already rides it.)

## What Changes

- `ExtensionPoint::PaletteAction` joins the host contract (with its `surface:palette` grant).
- Accepted PaletteAction contributions appear as command-palette rows keyed `plugin:<plugin>:<contribution>` — a namespaced, contract-negotiated class dispatched _before_ `Action` lookup, so the "every palette key is an Action" invariant keeps holding for everything else (no string back door: the key encodes its owner and the owner was negotiated).
- Invocation: resident plugins receive `on_event` (`kind: Action`, `payload.id` = the contribution id); one-shot plugins run once immediately (`PluginsHost::run_one_shot`, off-loop, result applied like a scheduled run). Disabled plugins vanish from the palette and refuse invocation with a status line.
- Help page + extending doc updated.

## Impact

- Audit row C2. Specs: plugin-runtime delta.
- Code: thegn-svc loader (contract), thegn-host (plugins.rs, handlers/plugins.rs, run.rs palette build + dispatch arm), docs.
