# THE-734 plan
Scope: sandbox.rs mounts_match/judge_inspect/parse_apple_inspect_sealed.
- `emits_nix_bind(spec)` shared with oci_create_opts; /nix tolerated only if true.
- Apple sealed: missing configuration.mounts array => not reusable.
- Docs: docs/help/sandboxing.md, openspec sandbox spec.
Tests: sandbox_tests.rs (nix tolerance, apple missing mounts).
