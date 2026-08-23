# Write a plugin

Plugins are out-of-process programs speaking newline-delimited JSON
(`thegn_core::plugin_api`, `openspec/specs/plugin-api`). Any language works;
a POSIX shell script with `printf` is enough.

1. **Manifest**: declared, not printed — a `[[plugins]]` entry or a
   `<config_dir>/plugins/<name>/plugin.toml` with `id`, `name`, `version`,
   `api` (the contract version, e.g. `"0.2.0"`), `capabilities`
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
   token — `host:sessions.list` needs `read`, `host:worktrees.open` needs
   `write`.

**Gates:** `docs/api/plugin-api-<version>.json` (a wire change without an
`API_VERSION` bump fails `plugin_api_wire`), `plugin_host_calls_cover_catalog`
(a host verb must exist in the capability catalog), `config_example` if you
add config surface.
