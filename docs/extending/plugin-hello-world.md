# Plugin hello world

## What you build

This walkthrough creates a one-shot plugin that puts “hello from a plugin” in
the status bar. Optionally, you can also preview a resident app-tab plugin
without opening the full UI. Plugin processes are native and **unsandboxed**:
they run as you, with normal OS access, so install only plugins you would run
in a terminal. `thegn plugin check` validates a manifest; it does not contain
the process.

## Step 1: create the directory

Directory plugins live under the configuration directory. Create the plugin
folder by hand; there is no scaffold command. The repo’s examples are the
templates.

```sh
CFG_DIR="$(dirname "$(thegn config path)")"
mkdir -p "$CFG_DIR/plugins/hello-world"
```

## Step 2: the manifest

Create `$CFG_DIR/plugins/hello-world/plugin.toml`. Directory plugins use their
own directory as the default working directory, so `hello.sh` can sit beside
the manifest and the command does not need a `../` path.

```toml
id = "hello-world"
name = "Hello world"
version = "0.1.0"
api = "0.4.0"
capabilities = ["surface:statusbar"]
command = ["sh", "hello.sh"]
mode = "one_shot"

[[contributions]]
id = "hello-world.seg"
extension_point = "StatusBarSegment"
label = "Hello"
surface = "hello-world.segment"

[contributions.cadence]
kind = "interval"
millis = 5000
```

The contribution id and surface identify what the script registers and
updates. Keep them consistent between this file and the script.

## Step 3: the script

Create `$CFG_DIR/plugins/hello-world/hello.sh` with these two lines:

```sh
printf '%s\n' '{"method":"register","params":{"id":"hello-world.seg","extension_point":"StatusBarSegment","label":"Hello","surface":"hello-world.segment","cadence":{"kind":"interval","millis":5000}}}'
printf '%s\n' '{"method":"update","params":{"surface":"hello-world.segment","view":{"spans":[{"text":"hello from a plugin","role":"Accent"}]}}}'
```

Each line writes one NDJSON message to stdout. The `register` message claims
the contribution declared in the manifest; its `id` must match
`hello-world.seg`. The `update` message fills the `hello-world.segment` surface
with the status-bar text. Lines that are not valid JSON are retained as junk
for diagnostics, so keep stdout for protocol messages.

## Step 4: check it

Run the inspection commands:

```sh
thegn plugin list
thegn plugin check
```

`thegn plugin list` shows a `hello-world` row. A successful check ends with
`plugins: ok (` followed by a discovered count and set digest, then `)`. The
count and digest depend on your configuration. `thegn plugin check` validates
the plugin but does not start it.

Once the plugin is enabled and its contribution is accepted, the host renders
its status-bar segment automatically in the free space between the left and
right status-bar clusters; no placement setting is needed. A segment that does
not fit is skipped. See [Plugins](../help/plugins.md) and [Masthead & status
bar](../help/bars.md) for details.

## Step 5 (optional): a resident app tab

Use `examples/plugins/cell-grid/` as the template. Copy that directory to
`$CFG_DIR/plugins/cell-grid`; it contains the `plugin.toml` manifest and
`grid.py` script. From the repository root, run:

```sh
cp -R examples/plugins/cell-grid "$CFG_DIR/plugins/cell-grid"
```

Then preview it without opening the full UI:

```sh
thegn plugin preview --plugin-dir "$CFG_DIR/plugins/cell-grid" --headless
```

The headless preview prints a JSON snapshot whose `live` object reports
`plugin` and `surface` identities, the current `revision`, the count of
`rejected_views`, and the count of `junk_lines`. Without `--headless`, the
preview is interactive; press `Ctrl+Q` to close it.

## Going further

- [plugin.md](plugin.md) covers the manifest, lifecycle, host calls, and scopes.
- [plugin-apps.md](plugin-apps.md) covers app tabs, preview, the SDK, and conformance.
- The [Plugins help page](../help/plugins.md) describes user-facing plugin behavior.

The Rust SDK is in the source tree at `crates/thegn-plugin-sdk` and is not
published as a crate. For the widget gallery, run
`thegn plugin preview --fixture gallery`.
