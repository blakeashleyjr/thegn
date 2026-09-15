# THE-603 strict state capture: source checkpoint

This is a standalone, unwired capture component atop THE-602. Current compilation,
native execution, strict lint, independent review, and local-main delivery remain
pending. Historical retained-candidate test counts are not current acceptance.

The implementation starts from corrected THE-602 `bb97ec24e6d679400b4e97615e4e27d2c4a97039`.
It recovers only the additive capture component from retained
`156ed688d1f21c9f3a9285a992f3bed65ff0eefc`, then strengthens absence classification
and fixture evidence. The source-neutral archived-canary guard correction
`393473624691b0aee04b2725b762e06210c51609` is included separately. Existing THE-602
composition, strict schema/JSON decoding, transaction semantics, Cargo dependencies,
legacy DB readers, launch paths, and other delivery owners remain unchanged.

## Accepted source boundary

Core opens the selected absolute filename with SQLite's explicit Unix VFS,
read-only/no-follow/private-cache/no-mutex flags, checks read-only state, sets a
250ms SQLite busy timeout, calls THE-602's owned strict read transaction, and
closes the connection before returning copied definitions or a static typed error.
It never creates the base, migrates, prunes, installs policy, or uses immutable/
URI/procfd-only substitution. Normal SQLite SHM/WAL bookkeeping remains permitted.

The private Linux adapter inspects the root, components, base and adjacent
WAL/SHM/journal objects using O_PATH descriptors. Its supported stable namespace
requires regular types, current-effective-UID/root ownership, no group/other
write permission, and actual ext2/3/4, XFS, Btrfs or tmpfs classification. Existing
unsupported or unreadable state refuses. A missing lookup retains its ancestor
chain and exact first missing name. Final verification detects changed ancestors
or newly created names; any orphan sidecar beside a missing base refuses without
SQL or deletion. No such adapter is wired into startup or launch admission.

These descriptors do not bind SQLite's independently opened inode. Pre/post
observations do not prove ABA resistance, atomic absence, resistance to hostile
same-UID/root/mount-admin replacement, or a raced FIFO substituted during SQLite
open. The future caller must establish a non-workload-writable namespace. The busy
timeout bounds ordinary lock waiting, not total I/O, cancellation or shutdown.
Non-Linux returns Unsupported for this new component; this is neither native
Windows/macOS acceptance nor a change to existing platform startup behavior.
THE-592/THE-607 retain final composition, visible launch Hold, authority and worker
integration obligations. No live state DB, provider, mount or privileged action
was used to author this checkpoint.

## Native validation plan, not executed

Run the actual core `host_db_capture::tests::` module plus existing THE-602
`host_db_snapshot::tests::` and strict decoder/composition coverage. The five new
core tests exercise latest committed uncheckpointed WAL data; unchanged base/WAL
bytes and unrelated sentinel data; missing/empty/corrupt input; malformed JSON,
old/new/current schema and missing hosts table; literal query-like names; real
exclusive-lock Busy and successful recovery. A build without Unix VFS explicitly
asserts refusal; it cannot count as positive WAL coverage.

Run the 16 actual Linux host tests in the
`platform::state_db_capture::linux::tests::` module. Its private rooted fixtures
verify their real supported filesystem and exercise absent leaves/parents;
static symlink/FIFO/socket/directory/device refusals; unsafe modes and sidecars;
WAL capture and typed failures; pre/post-read replacement; existing sidecar loss;
base/intermediate-parent creation and ancestor swap/mode-change rendezvous;
each orphan suffix both before and after initial lookup; special orphan objects;
real chmod-denied parent/file followed by descriptor-based restoration and fresh
successful capture; and permission restoration during assertion unwind.

Permission fixtures require a genuinely unprivileged Linux runner, and unsupported
filesystem/premise failures remain visible. The private test root is not proof
that production accepts `/tmp`: the production path walks from `/` and explicitly
refuses its shared writable ancestor. Successful private fixtures explicitly close
their directories after closing SQLite writers. Ordinary assertion unwind retains
permission-restoration guards; tests do not change HOME or ambient configuration.

Use finite test-runner deadlines. No historical infinite loop, live database,
provider or namespace attack is required. Two separate source counterfactuals can
prove the new regression boundaries: (1) bypass final missing-name/ancestor
verification while retaining the creation/swap fixtures, and (2) omit only orphan
sidecar refusal while retaining all suffix/special-object fixtures. Each should
fail its exact semantic assertion, not compilation; no mutation has been run.

## September 15 verification and standalone landing

The corrected component at `38a1f84d` is extracted onto local-main base `59a9c944`. All five added implementation/fixture files and the three strict host-capture dependencies match corrected tested source `dd5e4d07` exactly. The initial combined06 focused run passed 121 tests, including five core and sixteen Linux capture fixtures. After the Linux-only lint correction, all sixteen Linux fixtures passed again; strict workspace/all-target Clippy also passed. The original lint failure and corrected receipts are preserved together.

Two separate counterfactual builds each detected the intended regression. Removing final missing-ancestor verification fails the first ancestor-replacement assertion after permission restoration and the zero-SQL counter. Ignoring orphan sidecars fails the first regular WAL case with `Absent` after the zero-SQL counter. Both finish in 0.07 seconds under an outer 30-second bound. Later cases and explicit strict directory close are unreached on these failing paths; ordinary fixture destructors run. Neither counterfactual is a shipping change.

Current standalone source ratchets, strict OpenSpec validation and formatting passed. The durable [evidence manifest](the603-2026-09-15/manifest.json) preserves source comparisons, positive/native/lint receipts, original failures and counterfactual provenance. Final independent standalone review approved the source, evidence and limits. Reviewed local-main landing completed at `c6191b2a3ee84a4696e772edc9a5a0ab8d04ebff` on September 15. No new full-suite execution is claimed for this extraction.

The stable-namespace, Linux-premise, permitted SQLite sidecar and unwired integration limits above remain in force. THE-592/THE-607 own launch/worker integration. A future PR must run the repository-required `just ci`; no PR or full CI result is claimed here.
