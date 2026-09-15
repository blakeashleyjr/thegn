# THE-598 host environment precedence: scoped acceptance

Source base: `bb8ff6d77ba85ec292494957653b6c9d9047b8ed`; production repair is
included in combined `62335026`, with test-only correction `a4468a8e`. Native
precedence/compatibility and meaningful old-body counterfactuals are now
recorded. Strict Clippy and metadata validation passed; reviewed implementation landed on local main at
`c0d3d860a22db0a7fddafb3d338bf40b25248ade`.

The small `host_config::merge_host_defs` repair uses the effective map entry for
synthesized placement and SSH settings. Existing explicit env values remain
unchanged. Only this small repair was extracted from retained broader commit
`f3354d2c529d72f2222faadb69055fe69f790622`; no broader feature code is imported.

Primary and independent adversarial source review approved the production hunk
and five regressions. Formatting and whitespace checks passed. Offline reciprocal delivery validation
and strict validation of `preserve-winning-host-environment` also passed. The five tests
contain 19 cases, not 19 registered tests:

- `host_config::merge_tests::declared_local_shadows_db_ssh_in_resolved_environment`
- `host_config::merge_tests::declared_ssh_shadows_db_reaches_and_connection_settings`
- `host_config::merge_tests::declared_nonpane_reaches_never_synthesize_losing_db_environments`
- `host_config::merge_tests::explicit_environment_survives_host_definition_merge_and_resolution`
- `host_config::merge_tests::unshadowed_db_definitions_resolve_only_supported_pane_reaches`

Every case calls actual merge and environment resolution, verifying full host
or explicit-env preservation and exact resolved placement/SSH settings. Private
empty root/worktree directories and explicit local GitLoc avoid Git/DB discovery
and transport dispatch; strict directory close detects successful-path residue.
Unresolved Iroh/cloud selections retain existing diagnostics.

The first old-body attempt reused a fixed compiled artifact and reported two
passes without recompiling core. Its depfile named the combined checkout, so
that attempt is invalid counterfactual proof and remains in the record. Root
advanced only the mutant source mtime, retaining identical bytes. The retry
compiled the actual mutant checkout and produced a distinct binary and depfile.

Retry `25ba2d6f-c6ad-4dbe-af96-e19964bb3965` failed both intended selectors
(exit 100, 0.281 seconds): full synthesized SSH-settings equality found losing
DB settings rather than the declared winner. Actual environment resolution and
strict private-directory close precede the assertion. Later placement assertions
and remaining SSH-loop cases are not reached in the mutant; the positive suite
covers the full matrix. Both fixed and mutant depfile evidence is preserved,
with runtime binary hashes recorded but no binaries stored in this repository.
No live DB, SSH, provider, config-loader or global environment mutation was part
of verification; actual resolver reads of absent private repo overlays are
included.

This bounded repair does not establish THE-602 persisted-host capture, THE-603 DB
classification, or THE-592 checked final launch/receiver composition. Those
issues and any broader network-policy work remain separate.

## Segmented native evidence and remaining gates

[The evidence manifest](maintenance-04-2026-09-15/manifest.json) preserves original receipts, logs and independent reviews byte-for-byte. Combined `62335026` focused acceptance passed 68/68. Its full configured native run completed 8377 tests: 8376 passed, one host-key-literal ratchet failed, and 26 configured tests were skipped. Test-only `a4468a8e` replaces the precedence fixture's HostKeyAlias argument with distinct ConnectTimeout values; its fresh six-test gate passes all five precedence tests and the formerly failing ratchet. These are overlapping, segmented gates, not a single clean full-a446 run.

Source ratchets, 159 strict OpenSpec items, treefmt and non-Rust lint passed. Final metadata ratchets/OpenSpec checks and strict offline workspace/all-target Clippy also exited zero; Clippy took 13m58s, with inherited dependency warnings retained in the raw log. [The final gate receipt](maintenance-04-2026-09-15/thegn-maintenance04-final-gates-receipt-20260915.json.raw) records exact commands, source and log hashes. Reviewed implementation landed on local main at `c0d3d860a22db0a7fddafb3d338bf40b25248ade`. [The landing readback](maintenance-04-2026-09-15/thegn-three-fix-local-main-readback-20260915.json.raw) verifies all 16 reviewed Rust hashes, the 40 pre-landing evidence payloads and preserved user files. Tracker closure/readback remains pending; no new performance acceptance is claimed.
