# Bounded logical read-only capture

The core helper explicitly selects SQLite's Unix VFS with READONLY, NOFOLLOW,
PRIVATE_CACHE and NOMUTEX, checks main read-only state, captures THE-602 data,
and explicitly closes the connection. It returns only copied data or static
typed Open/Busy/NotReadOnly/Close/Definitions errors, never a reusable connection.
No URI parsing, immutable mode, migrations, journal-mode changes or policy
installation occur. The owned 250ms busy timeout bounds lock waiting only.

The private host helper is deliberately unwired. Linux O_PATH component handles
inspect the original absolute filename and existing WAL/SHM/journal sidecars,
without opening observed FIFOs/devices for I/O. Inspection is bounded to 4096
path bytes and 256 components. Pinned handles must identify the expected regular
file/directory types. URI/query-like spellings (including literal '?' and '#'),
controls, traversal and normalized-away path segments are refused explicitly.
Pinned handles must also have
current-effective-UID/root ownership and no group/other
write access. Only ext2/3/4, XFS, Btrfs and tmpfs are admitted; overlay, NFS,
CIFS, FUSE, 9p and unknown filesystem types are unsupported. On these Linux
POSIX ACL models named-entry effective permissions are masked by group mode
bits. Other platforms refuse; a symlinked/shared state root is not supported.

Assumptions remain explicit: ordinary concurrent SQLite writers honor SQLite
locking, and same-UID/root/mount-admin namespace attacks are outside this
contract. The future caller must independently establish non-workload-writable
state. Neither a mode check nor a path/boolean is that authority. This is not a
proof of the actual SQLite inode: stock VFS opens the original pathname again,
not the pinned O_PATH fd. Pre/post checks detect observed changes, not ABA or a
raced FIFO/device substituted within SQLite's independent open. No hard I/O,
shutdown or cancellation bound is claimed. Filesystem support checks do not
prove that arbitrary mounts are controlled by a trusted administrator.

A missing lookup retains the inspected ancestor descriptors, identities, and
exact first missing name. Before returning Absent, capture rechecks the chain
and that name. Creation of an intermediate directory is also a changed source,
even when its deeper leaf remains missing. When the first missing name is the
base file, any adjacent WAL, SHM or journal object (including a symlink or FIFO)
returns OrphanedSidecar without SQL reads or orphan deletion. A final missing
lookup is an observation, not an atomic namespace or freshness lease.

Normal SQLite locking and WAL/SHM support-file activity are allowed even though
SQL data is read-only. This is not a zero-filesystem-write/dry-run API. Existing
sidecar replacement/disappearance conservatively holds; newly created sidecars
must pass the same inspections. Creation by SQLite is not individually attested.
The original filename preserves WAL association; no copied main DB, procfd-only
SQLite filename, immutable URI or custom VFS is used.

Private Linux tests use real owned temporary directories under the runner's
private /tmp tree, with a cfg(test) rooted entrypoint and an actual supported
fstatfs premise. They exercise SQL and namespace policy, not a positive
production /tmp route: production always walks from '/', refusing its globally
writable ancestor. Actual fstatfs accept/reject tests remain separate; no fake
filesystem labels or privileged mounts are used. Unreadable parent/file controls
require an unprivileged runner and restore permissions through a descriptor
acquired before chmod, including assertion unwind. Successful paths explicitly
close their owned temporary directories. Core VFS tests assert
refusal on builds lacking Unix VFS, without claiming positive WAL coverage there.

THE-592 still owns captured-source/host-row composition, worker/coalescing and
result deadlines, boot-visible launch holds, receiver gates, and startup policy
installation ordering. No caller is activated by this standalone component.
