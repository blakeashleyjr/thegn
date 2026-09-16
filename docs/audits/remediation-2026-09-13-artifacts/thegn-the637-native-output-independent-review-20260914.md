# Independent source review: THE-637 and resident fixture output

September 14, 2026. Both scoped changes are source-approved with no remaining blocker. This review ran no Cargo, test binary, live Git/provider action or native child. Root owns compiled regression execution and final landing.

Reviewed in `/tmp/thegn-audit-remediation-20260913`:

- THE-637: the working diff in `crates/thegn-svc/src/git/mod.rs` based on integration `1a378694`, reviewed file blob `54e0674db7b5f92a7a017831de0a25cbe50cb81a`; exactly two test fixtures and adjacent helper comments change. Root may checkpoint this diff after this review.
- THE-154 fixture-output lint revision: commit `1a37869458f5c87021b35dead21025b98eec57c6`, `crates/thegn-svc/src/plugin/platform/native_tests.rs`.

## THE-637 findings and verdict

The pre-fix isolated reproduction receipt `/tmp/thegn-svc-git-fixture-reproduction-20260914.json` distinguishes two real fixture defects: the bare remote and clone HEAD pointed to `master` while only `main` existed, yielding `None` rather than an upstream result; the intended conflicting merge exited 128 for missing committer identity, so it never created MERGE_HEAD. These receipts support the narrow fixes rather than a product Git behavior change.

The ahead/behind fixture now initializes its private bare remote with explicit `-b main`, matching its seed branch. The existing assertions still exercise both actual Gix and CLI implementations for `(0,0)`, `(2,0)`, `(2,1)` and no upstream. It does not fabricate an upstream or weaken the result assertions.

The merge-state fixture now supplies author and committer identity on the actual conflicting merge command, as its setup helper already does for commits. It requires exit code 1, preventing infrastructure/setup failure from masquerading as the intended conflict. Actual merge-state and abort assertions remain. Existing signing/LFS overrides are retained; no developer global configuration is written.

Both fixtures now own random `TempDir` directories through assertion unwind, replacing predictable process-derived paths and preemptive `remove_dir_all`. All new fixture writes and local bare/clone/push/fetch paths remain inside those owned trees. No production Git implementation changes. Inherited global Git configuration is still possible through the existing helper; this patch removes the demonstrated branch/identity dependencies and does not claim complete arbitrary-environment isolation.

`git diff --check` passed for the reviewed Git fixture diff. Root must execute both exact tests with the isolated final runner and the final suite before counting them passed.

## Native resident fixture-output verdict

All three substitutions use `writeln!(std::io::stdout(), ...).unwrap()` in the private reexecuted helper: READY handshake, final JSON response, and callback frame. They preserve each format expression, the leading handshake newline, the trailing line terminator and each explicit flush. The child argv already uses `--ignored --exact <helper> --nocapture --test-threads=1`; bypassing libtest's capture-oriented `println!` machinery therefore preserves the intended actual pipe protocol. A failed write still fails the fixture instead of pretending delivery succeeded.

The invalid-UTF8 frame, EOF behavior, child watchdog, process ownership and production plugin code are unchanged. No lint suppression, product-output helper, alternate logging sink or protocol relaxation is introduced. The revised output code still requires root's native fixture and clippy gates; source review is not execution evidence.

THE-154's full process-tree containment and native Windows execution acceptance remain open as previously recorded. This lint revision does not add a containment or platform guarantee.
