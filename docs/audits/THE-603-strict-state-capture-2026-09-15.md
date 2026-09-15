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

## Remaining gates

Current staged-file `just ratchets` (including delivery validation/fixtures),
strict scoped OpenSpec validation, targeted rustfmt, and scoped narrative treefmt
passed. Current primary/independent source review, coordinated native tests and
THE-602 compatibility, strict affected-workspace lint, and local-main landing
remain pending and must retain exact source and receipt provenance. Cross compilation, if later performed, remains a separate
compile-only receipt. Full parent authority and arbitrary-platform claims are
outside this scoped component. A future PR must run repository-required `just ci`;
no PR or full CI result is claimed here.
