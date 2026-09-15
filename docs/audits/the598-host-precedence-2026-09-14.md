# THE-598 host environment precedence: acceptance draft

Source base: `bb8ff6d77ba85ec292494957653b6c9d9047b8ed`. Native execution and
local-main delivery are pending; this draft claims only implementation and
source review.

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

Pending receipts: five native tests plus compatibility selectors, old-body
counterfactual, source/delivery/OpenSpec and applicable lint/CI gates, and reviewed
local-main hash. Counterfactual mutation must restore only the old merge body,
retaining the new tests; the local/SSH conflict must fail meaningfully. No live
DB, SSH, provider, config-loader or global environment mutation is part of this
verification. Actual resolver reads of absent private repo overlays are included.

This bounded repair does not establish THE-602 persisted-host capture, THE-603 DB
classification, or THE-592 checked final launch/receiver composition. Those
issues and any broader network-policy work remain separate.
