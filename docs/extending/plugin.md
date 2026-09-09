# Write a plugin

Plugins are out-of-process programs speaking newline-delimited JSON
(`thegn_core::plugin_api`, `openspec/specs/plugin-api`). Any language works;
a POSIX shell script with `printf` is enough.

1. **Manifest**: declared, not printed — a `[[plugins]]` entry or a
   `<config_dir>/plugins/<name>/plugin.toml` with `id`, `name`, `version`,
   `api` (the contract version, currently `"0.3.0"`), `capabilities`
   (`surface:statusbar`, …), `contributions` (extension point + label +
   surface + cadence), `command = ["…"]`, and optional `scopes`, `mode`
   (`one_shot`/`resident`), `timeout_secs`. Directory plugins default their
   `cwd` to their own directory.
2. **Validate it**: `thegn plugin list` shows what the loader sees;
   `thegn plugin check` negotiates against the host contract and exits
   non-zero on problems. Start from `examples/plugins/hello.sh` +
   `examples/plugins/hello/plugin.toml`.
3. **Lifecycle**: read `activate` / `render` / `on_event` / `deactivate`
   notifications on stdin; reply to `id`-bearing requests with
   `{"id":n,"result":…}` or `{"id":n,"error":{"code":…,"message":…}}`; send
   `update` / `notify` / `state.set` / `host.call` as you need.
4. **Host calls** are checked against your `scopes` exactly like a control
   token — `sessions.list` needs `read`, `worktrees.open` needs `write`, and
   exposed predeclared tools need `exec`. `write`, `git`, and `exec` are
   independent; `admin` implies every scope. Contribution capabilities do not
   grant a host-call scope. `tools.run` is callable today; `launch.preset`
   remains CLI-first and is not advertised to plugins until it has a generic
   control route.
5. **Provider plugins**: declare an `IssueProvider` contribution (with
   `capabilities = ["surface:provider"]`), an optional `caps` object, and
   answer `provider.call` requests —
   `{"id":n,"method":"provider.call","params":{"seam":"issues",
"op":"list_issues","args":{…}}}` — with the op's result (the issue
   seam's JSON shapes) or `{"code":"unsupported"}` for ops you don't
   implement. Omitted or `null` issue caps are all false; a false-cap optional
   operation is refused locally and does not round-trip. The contribution's
   `label` becomes the account name in the panel. See the complete
   [tracker-provider recipe](tracker-provider.md).

## Current host support (API 0.3)

The checked-in JSON schema carries this same table as
`x-thegn-extension-support`; `thegn plugin check` applies it without launching
the plugin.

| Extension point      | State            | Required capability    | Modes              | Cadence             | Owner                     |
| -------------------- | ---------------- | ---------------------- | ------------------ | ------------------- | ------------------------- |
| `StatusBarSegment`   | wired            | `surface:statusbar`    | one-shot, resident | on-demand, interval | general compositor        |
| `PanelSection`       | reserved         | `surface:panel`        | —                  | —                   | THE-108                   |
| `SidebarTab`         | reserved         | `surface:sidebar`      | —                  | —                   | THE-107                   |
| `PaletteAction`      | wired            | `surface:palette`      | one-shot, resident | on-demand           | general compositor        |
| `NotificationSource` | wired            | `surface:notification` | one-shot, resident | on-demand, interval | general compositor        |
| `HarnessAdapter`     | reserved         | `surface:harness`      | —                  | —                   | `add-agent-harness-seam`  |
| `ProgramAdapter`     | reserved         | `surface:program`      | —                  | —                   | no accepted runtime owner |
| `Theme`              | reserved         | `surface:theme`        | —                  | —                   | THE-107                   |
| `Automation`         | reserved         | `surface:automation`   | —                  | —                   | native engine only        |
| `DataSource`         | separate adapter | `surface:data`         | —                  | —                   | calendar command accounts |
| `IssueProvider`      | wired            | `surface:provider`     | resident           | on-demand           | issue provider bridge     |
| `CiProvider`         | reserved         | `surface:provider`     | —                  | —                   | no dynamic selector       |
| `ForgeProvider`      | reserved         | `surface:provider`     | —                  | —                   | no dynamic selector       |

The host verb matrix is also embedded as `x-thegn-host-verb-support`, and the
exact currently dispatchable catalog ids are in
`x-thegn-host-call-capabilities`. The runtime implements `register`, `update`,
`invalidate`, `io`, `notify`, `emit`,
and `state.set` for both modes. `subscribe`, `state.get`, `host.value`, and
`host.call` require a resident reply/event channel. `host.call` can reach only
non-streaming, non-admin capabilities explicitly listed for the plugin surface
and backed by the generic control route spine; event streams use the resident
`on_event` bridge. In particular, `tools.run` is the current `exec`-scoped
call, while the CLI-first `launch.preset` is excluded.

Plugin scopes constrain calls made through the host. They do not sandbox the
plugin subprocess itself; installing a plugin grants that executable the same
ambient OS access it would have when launched directly.

**Gates:** `docs/api/plugin-api-<version>.json` (a wire change without an
`API_VERSION` bump fails `plugin_api_wire`), `plugin_host_calls_cover_catalog`
(a host verb must exist in the capability catalog), `config_example` if you
add config surface.
