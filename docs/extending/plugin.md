# Write a plugin

Plugins are out-of-process programs speaking newline-delimited JSON
(`thegn_core::plugin_api`, `openspec/specs/plugin-api`). Any language works;
a POSIX shell script with `printf` is enough.

1. **Manifest**: the first line you print is `{"method":"manifest","params":{…}}`
   with `id`, `name`, `version`, `api` (the contract version, e.g. `"0.2.0"`),
   `capabilities` (`surface:statusbar`, `notify:<source>`, `state:<id>`,
   `host:<capability id>` …) and `contributions` (extension point + label).
2. **Declare it** in `[[plugins]]` (`id`, `name`, `version`, `api`,
   `command = ["…"]`, optional `scopes`, `mode`, `timeout_secs`).
3. **Lifecycle**: read `activate` / `render` / `on_event` / `deactivate`
   notifications on stdin; reply to `id`-bearing requests with
   `{"id":n,"result":…}` or `{"id":n,"error":{"code":…,"message":…}}`; send
   `update` / `notify` / `state.set` / `host.call` as you need.
4. **Host calls** are checked against your `scopes` exactly like a control
   token — `host:sessions.list` needs `read`, `host:worktrees.open` needs
   `write`.

**Gates:** `docs/api/plugin-api-<version>.json` (a wire change without an
`API_VERSION` bump fails `plugin_api_wire`), `plugin_host_calls_cover_catalog`
(a host verb must exist in the capability catalog), `config_example` if you
add config surface.
