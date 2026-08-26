# Tasks — generic LSP registry

## 1. Config (thegn-core)

- [x] 1.1 Extend `LspServerConfig` with optional `extensions: Vec<String>` and
      `language_id: Option<String>` (serde-defaulted so every existing config
      parses unchanged). Keep `lang`/`command`/`args` semantics intact,
      including `command = ""` as the disable form.
- [x] 1.2 `config_validate`: a non-built-in `lang` key with no `extensions` is
      an error; an extension claimed by two entries is flagged (named in the
      message); a built-in key with `extensions` overrides the default set.
- [x] 1.3 Unit tests: round-trip the new fields, the validation errors, and
      that a legacy override-only entry deserializes identically.
- [x] 1.4 Document the extended `[[lsp.servers]]` keys in
      `config/config.toml.example` (a built-in override example AND a
      non-built-in example, e.g. `zls` for Zig). The generated
      config-reference help page picks the keys up automatically.

## 2. Registry resolution (thegn-svc)

- [x] 2.1 Express the six built-ins as data: a `builtin_servers()` table of
      registry entries (key, extensions, language_id, default command/args)
      replacing the `lang_key`/`language_id`/`default_command` enum matches.
- [x] 2.2 Merge user entries over built-ins field-wise (override command
      trusted outright; built-in default used only when its binary is on
      PATH; `command = ""` disables) — preserving every current
      `resolve_server` behaviour for the six known keys.
- [x] 2.3 Extension → registry-key resolver (`resolve_key(path) ->
Option<String>`), first-declared wins on collision. Pure; unit-tested
      including collision order, case-insensitivity, and unknown extensions.
- [x] 2.4 Keep `Lang` as the tree-sitter tier: `Lang::from_key(&str)` adapter
      linking a registry key to its grammar when one exists; unit tests pin
      the six mappings and `None` for registry-only keys.

## 3. Capability negotiation (thegn-svc)

- [x] 3.1 Retain the `initialize` result's `capabilities` object on
      `LspClient` (parsed defensively; absent/malformed ⇒ empty = everything
      gated off except the handshake).
- [x] 3.2 Pure `supports(method, &caps) -> bool` over the seven provider
      fields, tolerating the bool-or-object union. Unit-test against
      real-world capability shapes (rust-analyzer, pyright, gopls, a
      minimal server).
- [x] 3.3 Gate every request method: undeclared capability ⇒
      `LspError::NotAvailable` without touching the wire. Extend the
      `fake_lsp` harness + `lsp_client.rs` tests: a server declaring no
      `hoverProvider` never receives a `hover` request.

## 4. Supervisor + ceilings (thegn-host)

- [x] 4.1 Rekey `ClientMap` to `(PathBuf, String)` (registry key); spawn,
      initialize, failure-cache, warm-reuse, and shutdown-on-drop behaviour
      unchanged. Update the supervisor tests.
- [x] 4.2 Wrap local server spawn argv with
      `sandbox_cpucap::wrap_background_argv` so servers join `thegn.slice`;
      fail-safe (unpublished policy / unusable systemd-run ⇒ unwrapped, as
      before). The bridged (`from_io`) transport is untouched.
- [x] 4.3 Trust gating: `[[lsp.servers]]` entries sourced from a
      worktree-layer config file are ignored with a one-time status-line
      notice (until `add-config-trust-resolution` lands and takes over).

## 5. Consumer sweep (thegn-host)

- [x] 5.1 Move consumers from `Lang`-keyed to registry-key-keyed client
      lookup: Problems (diagnostics), Symbols section outline fetch,
      hover/signature/code-action, Search Everywhere symbol mode,
      go-to-def/refs.
- [x] 5.2 Per-tier degradation: registry-only languages take the LSP path
      with no tree-sitter fallback (outline empty-state message rather than
      a wrong parse); unregistered languages keep today's regex fallbacks.
      The semantic-graph builder keeps requiring the tree-sitter tier
      (unchanged, per the design).
- [x] 5.3 `NotAvailable` from the negotiation gate flows into each consumer's
      existing missing-server fallback (no new consumer plumbing) — verify
      with targeted tests per consumer.

## 6. Doctor (thegn-host)

- [x] 6.1 `thegn doctor` LSP section: every registry entry (built-in +
      user) with key, extensions, resolved command or `missing`, and
      whether `[lsp].enabled` / `command = ""` masks it. Existence-only —
      no server is spawned. Mirror into the doctor JSON output.
- [x] 6.2 Unit test the section's resolved-list formatting (pure part) and
      extend the smoke test to assert the section prints.

## 7. Docs + validate

- [x] 7.1 Update `docs/help/configuration.md` prose for the registry (the
      keybindings/config-reference pages are generated — do not hand-edit).
      No new action/keybind/zone ⇒ no help-ratchet entries expected.
- [x] 7.2 `git add` any new modules before nix-build (flake source allowlist
      sees only git-tracked files).
- [ ] 7.3 Run `just ci` once, when the change is complete (includes
      openspec-validate). _(Deferred to the reviewer's pre-PR gate — this change
      was implemented under a no-full-workspace-gates policy; scoped clippy +
      nextest were run instead.)_
