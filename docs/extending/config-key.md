# Add a config key

1. **Declare it** on the owning struct in `crates/thegn-core/src/config*.rs`
   with a doc comment (it becomes the schema description) and a `Default`.
   Enumerated values use `config_enum!` — never a bare `String` with a
   documented vocabulary — and a value that is accepted but not implemented
   is marked `reserved`.
2. **Document it** in `config/config.toml.example` under its `[section]` as
   `# key = default  # what it does`. The config-reference help page is
   generated from this file at runtime.
3. **Env knob (optional but preferred for anything a CI job would flip):**
   one line in `Config::env_overlay` (`crates/thegn-core/src/config.rs`),
   named `THEGN_<SECTION>_<KEY>`, and a pair in the
   `env_overlay_covers_every_knob` test. Otherwise pin the key in
   `test/env-overlay-ratchet.txt` with a reason.
4. **home-manager (optional):** add the option and its `snake_key = cfg.camelKey`
   line in `nix/hm-module.nix`.
5. **Tests** for any logic (thegn-core is 95%-line gated).

**Gates:** `config_example` (undocumented key), `env_overlay_coverage`
(no knob and not pinned; knob not exercised), `hm_module_drift` (nix renders
a key the schema lacks, or offers an enum value the binary rejects),
`marked_definition_count_is_pinned` (a new `config_enum!` bumps the pin
deliberately), `config validate --strict` (reserved / unknown keys).
