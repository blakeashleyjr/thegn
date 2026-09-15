# Validation checkpoint

Source implementation is frozen in a private candidate. No Cargo command,
build, test process, provider, or real Git fixture was run for this candidate.

Root-owned follow-up selectors are the Unix
`platform::gate_path::tests::exclusive_regular_creation_never_adopts_an_existing_leaf`,
`platform::gate_path::tests::exclusive_regular_creation_has_one_winner_across_two_owned_children`,
`merge_remote::tests::bundle_cleanup_preserves_an_observed_replacement`, and
`merge_remote::tests::bundle_custody_rejects_special_replacements_and_keeps_modes`,
`merge_remote::tests::bundle_write_revalidation_failure_preserves_replacement`,
`merge_remote::tests::bundle_and_fetch_moves_a_tip_between_stores`,
`merge_remote::tests::bundle_creation_collision_preserves_preexisting_leaf`,
`merge_remote::tests::bundle_write_and_flush_failures_finish_without_recursive_cleanup`,
and `merge_remote::tests::bundle_unwind_after_injected_write_failure_cleans_owned_fixture`
tests. The shipping fetch fixture uses an explicitly private temporary root
and asserts both successful fetch and failed fetch leave it empty. The
Windows create-new/owner-SID DACL/reparse/share path requires a Windows runner
and is pending; its source path uses stable `GetFileInformationByHandle`
identity rather than unstable standard-library metadata methods.

The source preserves the existing observation boundary: identity revalidation
refuses an observed replacement but is not an atomic lease against arbitrary
same-UID changes. Remote synchronization and byte-streaming follow-ups remain
outside THE-222.
