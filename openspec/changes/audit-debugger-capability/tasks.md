# Tasks — debugger audit + adapter registry

## 1. Config (thegn-core)

- [ ] 1.1 `[[debug.adapters]]` config type: `name`, `run`/`attach` argv
      templates, `platforms` list. Serde-defaulted; empty table means
      built-in `bs` only.
- [ ] 1.2 `config_validate`: unknown template placeholders, duplicate
      adapter names, a `name = "bs"` entry overriding built-in fields, and
      malformed platform strings are errors with named messages.
- [ ] 1.3 Document the table in `config/config.toml.example` (one commented
      lldb example) — picked up by the generated config-reference page.
- [ ] 1.4 Unit tests: round-trip, defaults, each validation error.

## 2. Pure adapter logic (thegn-core `debug.rs`)

- [ ] 2.1 Adapter model: built-in `bs` entry as data (managed-tool `{bin}`,
      Linux-x86-64 platforms) merged with user entries.
- [ ] 2.2 Template substitution (`{program}`, `{pid}`, `{bin}` built-in
      only; trailing debugee args appended on `run`) — pure, replacing the
      hand-built `launch_argv`/`attach_argv` while keeping their exact `bs`
      output (pin with the existing tests).
- [ ] 2.3 Per-adapter platform gate: pure predicate over the entry's
      `platforms` + `(os, arch)`; refusal reason names the adapter and its
      platforms. `bs` behaviour byte-identical to today's message intent.
- [ ] 2.4 Unit tests to the core gate: substitution, unknown adapter,
      per-adapter gating, `bs` argv regression.

## 3. CLI (thegn-host `cmd/debug.rs`)

- [ ] 3.1 `--adapter <name>` on `run`/`attach` (default `bs`); unknown name
      refused listing known adapters + a config pointer.
- [ ] 3.2 User-adapter resolution: argv[0] via PATH/absolute; `{bin}`
      (managed resolution + auto-install at the pin) remains `bs`-only.
      `thegn debug path` reports the selected adapter's resolution.
- [ ] 3.3 Trust gating: worktree-layer `[[debug.adapters]]` entries ignored
      with a notice until `add-config-trust-resolution` lands.
- [ ] 3.4 Bump `BS_PIN` 0.4.6 → current (0.4.8 at audit time); verify
      `thegn debug setup --force` installs it.

## 4. Doctor (thegn-host)

- [ ] 4.1 Extend the doctor's debugger reporting: keep the `bugstalker`
      managed-tools row + platform note (now spec-backed), add one row per
      configured adapter with its resolution state and platform gate.
      Mirror into doctor JSON.
- [ ] 4.2 Test the pure formatting; extend the smoke test to assert the
      adapter rows print.

## 5. Docs + validate

- [ ] 5.1 Update `docs/help/` CLI prose for `--adapter` and the config
      table (no new action/keybind/zone ⇒ no help-ratchet entries
      expected; verify the ratchets stay green).
- [ ] 5.2 `git add` new modules before nix-build (flake source allowlist).
- [ ] 5.3 Run `just ci` once, when the change is complete (includes
      openspec-validate).
