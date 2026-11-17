# Private Workspaces

## ADDED Requirements

### Requirement: Workspace mode is chosen per workspace, independent of the backend

thegn SHALL record a `workspace.mode` of `private-copy`, `git-worktree` or `live-mount` for every worktree,
and SHALL treat it as a decision separate from the execution backend. Every launch MUST show workspace mode,
actual boundary, home exposure, shared writable mounts and effective egress as independent facts. Changing
mode SHALL create a new workspace generation and MUST NOT alter a running pane or delete the previous copy.
The user's own interactive panes SHALL keep `git-worktree` and the `standard` profile by default; only
untrusted launch classes default to `private-copy`, and only once their route is admitted and a qualified
backend exists. Private-copy workspaces SHALL be creatable only behind the `private_copy` feature gate until
that default flips.

#### Scenario: Interactive panes keep git-worktree

- **WHEN** the user opens a worktree shell or interactive agent pane with no explicit workspace choice
- **THEN** the pane uses `git-worktree` mode and the `standard` profile, and no private copy is captured

#### Scenario: Mode and backend are reported separately

- **WHEN** a private-copy workspace is launched on a host process backend
- **THEN** the launch reports workspace mode `private-copy` and the backend's actual boundary as separate facts, and does not claim confinement

#### Scenario: Changing mode does not touch the running pane

- **WHEN** the user changes the mode of a worktree that has a running pane
- **THEN** a new workspace generation is created, the running pane is unchanged, and the previous copy is retained

#### Scenario: An upgrade keeps an explicit compatibility choice

- **WHEN** a configuration that explicitly selects `git-worktree` for a class is read after the default flip
- **THEN** the explicit choice is honoured, while a fresh install gets `private-copy` for the untrusted classes

### Requirement: A private workspace has a durable identity and split storage

thegn SHALL give each private workspace a random UUID under a private durable data root scoped by user and
repository identity. Storage SHALL be split into the runtime-writable repository and broker-owned metadata
(baseline manifests and blobs, transaction journals, review artifacts, lease records and audit receipts). Only
the repository MUST be mounted into a workload; the parent directory, host checkout, application state,
control sockets, source common Git directory and neighbouring copies MUST NOT be. A container writable layer
or tmpfs SHALL be ineligible as authoritative storage, and admission of a private-copy workload MUST refuse a
backend whose storage is ephemeral.

#### Scenario: Metadata is not mounted

- **WHEN** a private-copy workload starts
- **THEN** its mounts contain only the private repository, and the baseline, journal, receipts and leases are not reachable from inside the workload

#### Scenario: Ephemeral storage is refused

- **WHEN** a private-copy launch is requested on a backend whose only storage is a container writable layer
- **THEN** admission is refused and nothing is spawned on the host

### Requirement: Capture is explicit, stable and complete

thegn SHALL create a private copy only from an explicit capture of the selected host worktree, covering dirty
tracked files, staged and unstaged content as currently visible, and non-ignored untracked files, including
binary content and file modes. The host index MUST be recorded for diagnostics and left unchanged. Capture
SHALL verify identity and content twice or use a filesystem snapshot, and MUST abort or retry when input
changes instead of declaring a baseline. Capture SHALL record an immutable file baseline outside the workload:
relative path bytes, file type, executable mode, length, content hash or symlink target, and a tracked,
untracked or imported class, with the source checkout identity, base OID, host index fingerprint, capture
generation and ignore policy. Initially ignored untracked files SHALL be excluded by default, with their count
shown and inclusion an explicit user choice. A launch whose tree contains submodules or Git LFS content MUST
be refused unless the affected paths are explicitly excluded.

#### Scenario: The mixed fixture round-trips with an identical baseline

- **WHEN** a worktree with dirty, staged, untracked, binary and mode-changed files is captured
- **THEN** the private tree matches the worktree content and the recorded baseline is identical on a second capture of the same input

#### Scenario: Input changes during capture

- **WHEN** a source file changes between the two verification reads
- **THEN** capture aborts or retries and no baseline is recorded for the changed input

#### Scenario: Ignored files are counted but not copied

- **WHEN** the worktree contains ignored untracked files and the user has not included them
- **THEN** they are absent from the copy and the capture reports how many were excluded

#### Scenario: Submodule or LFS content

- **WHEN** the tree contains a submodule or Git LFS pointer files and no explicit exclusion is given
- **THEN** the private-copy launch is refused rather than reusing host submodule Git pointers or downloading data

### Requirement: Capture runs no source-repository executable configuration

Capture and history transport SHALL disable fsmonitor, hooks, filters, external diff, textconv and credential
helpers, SHALL use a broker-owned Git configuration, and MUST confine parser operations. thegn MUST NOT run an
ordinary host checkout or filter to construct the private copy.

#### Scenario: A hostile source configuration is inert

- **WHEN** the source repository configures a hook, a filter and an fsmonitor command that each write a canary file
- **THEN** capture completes and none of the canary files exist

### Requirement: A private repository is independent of its source

The private repository SHALL be a newly created Git repository that imports only the chosen base commit and
explicitly selected reachable history into newly owned objects, or a reviewed history-free initial commit. It
MUST NOT copy the linked `.git` pointer, `commondir`, source configuration, hooks, remotes, credentials,
worktree registrations, index, `objects/info/alternates` or unrelated refs, and MUST NOT use shared-object
clones or hardlinks. Before admission and at every reuse, thegn SHALL verify that `.git` is a real private
directory, that the Git common directory, object directory and refs resolve inside it, and that there are no
external alternates, symlinked metadata or shared object inodes; a failed check MUST refuse the launch.

#### Scenario: A linked pointer is rejected

- **WHEN** a private workspace's `.git` is a file pointing at the source common directory
- **THEN** the confinement check fails and the launch is refused

#### Scenario: An alternate is rejected at reuse

- **WHEN** a previously admitted private repository gains an `objects/info/alternates` entry
- **THEN** reuse is refused until the repository is repaired or recreated

#### Scenario: A shared inode is rejected

- **WHEN** an object file in the private repository shares an inode with the source repository
- **THEN** the confinement check fails and the launch is refused

#### Scenario: Source history stays out of the copy

- **WHEN** a copy is created with a history-free initial commit
- **THEN** the private repository contains no object reachable only from the source history

### Requirement: Sandbox history never reaches the host

Commits, refs, configuration and hooks of a private repository SHALL stay private. Only reviewed file changes
or a patch SHALL return to the host, through explicit sync, and thegn MUST NOT import sandbox refs, objects,
configuration or hooks into the host checkout. A successful sync SHALL NOT publish private commits or refs.

#### Scenario: A sandbox commit stays private after sync

- **WHEN** the agent commits in the private repository and the user syncs the resulting file changes
- **THEN** the host checkout has the file changes but no new commit, ref or object from the sandbox

### Requirement: Export is quarantined, bounded and never executes sandbox content

Export SHALL use a trusted, bounded reader and MUST NOT run a command proposed by the agent. Exports SHALL
land in quarantine, and paths, types and hashes MUST be validated before review. thegn SHALL reject absolute
and traversal paths, `.git` components, aliases to protected metadata, symlinked ancestors, duplicate
normalised names, device files, FIFOs, sockets, hardlinks to outside objects, unsupported destination names
and case or Unicode aliases on the actual host filesystem, and SHALL enforce per-file, aggregate, file-count
and expansion limits. The exporter MUST NOT follow a link out of the private tree. The host MUST NOT execute
Git from the sandbox repository with its configuration. The review artifact SHALL be a binary-capable patch
built from broker-owned baseline and candidate blobs in a sanitised temporary object store, plus a path, type,
mode and hash manifest and content blobs for binary additions, with no commit graph, refs, hooks or sandbox
configuration. Rendered names and content MUST neutralise terminal escapes.

#### Scenario: Hostile archive paths are rejected before review

- **WHEN** an export contains `../escape`, an absolute path, a `.git` component and two names that normalise to the same name
- **THEN** each is rejected before any review is presented and nothing is written to the host

#### Scenario: An escaping symlink is not followed

- **WHEN** the private tree contains a symlink whose target lies outside the tree
- **THEN** the exporter records the link without dereferencing it and rejects it on sync

#### Scenario: Review carries no repository metadata

- **WHEN** a review artifact is produced for a private workspace
- **THEN** it contains the patch, the manifest and binary blobs only, and no refs, hooks, configuration or commit graph

#### Scenario: Terminal escapes in filenames are neutralised

- **WHEN** a touched path contains a terminal escape sequence
- **THEN** the review renders it escaped and the terminal receives no control sequence

### Requirement: Sync applies nothing on conflict

For baseline `B`, candidate sandbox snapshot `S` and current host snapshot `H`, thegn SHALL compute the touched
paths where `S != B`, preserve host-only changes on untouched paths, and require `H == B` (absence, type and
mode included) on every touched path before accepting the change. A new sandbox path that collides with a new
host path MUST count as a conflict. A changed ancestor type, checkout identity, branch, HEAD or host index
state SHALL invalidate the reviewed transaction. On a conflict thegn SHALL apply nothing, keep both versions,
list the conflicting paths and offer re-capture, and MUST NOT auto-merge, force-overwrite, stage, commit,
cherry-pick or push. A successful textual patch check MUST NOT be treated as proof of safety. A review SHALL
be bound to the candidate's hashes, and later sandbox edits SHALL require a new candidate; the workload MUST be
frozen or snapshotted while a candidate generation is captured.

#### Scenario: A host edit on a touched path is a conflict

- **WHEN** the host file at a touched path differs from the baseline when the user chooses Sync
- **THEN** no path is applied, both versions are kept, and the conflicting path is listed

#### Scenario: Host-only changes are preserved

- **WHEN** the host has changed a path the sandbox did not touch
- **THEN** sync leaves that change in place and does not import it into the private tree

#### Scenario: HEAD or index movement invalidates the review

- **WHEN** the host HEAD or index moves after review and before apply
- **THEN** the transaction is invalidated and a new review is required

#### Scenario: A new path colliding with a new host path

- **WHEN** the sandbox and the host each created a different file at the same new path since capture
- **THEN** it is reported as a conflict and nothing is applied

#### Scenario: Later sandbox edits remain pending

- **WHEN** the agent edits a file after the candidate was frozen
- **THEN** the edit is not part of the review and remains pending against the baseline

### Requirement: Sync is journaled and recoverable

Before the first host mutation thegn SHALL write and fsync all candidate blobs and backups and SHALL persist
a transaction journal with preimages, desired hashes, destination identities and ordered operations. thegn
SHALL then apply each file through a same-filesystem temporary and an atomic rename, and fsync changed
directories, using no-follow directory-relative access and re-verifying every preimage under an application
lock. thegn MUST NOT report a tree as synced until every entry and the success receipt are durable. After a
crash, recovery SHALL distinguish old bytes, transaction-written new bytes and unexpected third-party bytes,
SHALL resume or roll back only known states, MUST halt and retain originals, candidates and backups on
unexpected bytes, and MUST NOT overwrite an edit made after the transaction. Transaction ids and receipts
SHALL make repeated sync idempotent, and the private sync baseline SHALL advance only for applied paths.

#### Scenario: A crash at every step recovers or halts safely

- **WHEN** a failpoint kills the process at each journal write, blob write, rename, directory fsync and receipt step in turn
- **THEN** recovery on restart either completes or rolls back to a known state, or halts with originals and backups retained, and the workspace shows `sync-recovery-needed` when it halts

#### Scenario: An unexpected edit during recovery

- **WHEN** a host edit lands on a path between the crash and recovery
- **THEN** recovery halts, keeps the unexpected bytes, the original and the candidate, and overwrites nothing

#### Scenario: Repeated sync is a no-op

- **WHEN** the user runs Sync again with no new private changes
- **THEN** nothing is written and the existing receipt is reported

#### Scenario: Concurrent sync is refused

- **WHEN** a second sync or a discard is requested while a sync transaction is open
- **THEN** it is refused until exclusive mutation admission is available

### Requirement: A private workspace is durable and recoverable

thegn SHALL persist workspace identity, generation, runtime and storage ids and lifecycle transitions outside
the workload, through the states `capturing`, `ready`, `running`, `stopped`, `review-ready` and `syncing`, with
the side states `capture-failed`, `runtime-lost`, `conflict` and `sync-recovery-needed`. A stopped or lost
runtime MUST NOT mean a discarded workspace. Container recreation, daemon pruning, warm-pool cleanup, dormancy
and application shutdown MUST NOT remove a private repository or journal, and a successful sync SHALL keep the
copy. A gateway crash or unavailable disk SHALL yield `runtime-lost` with a recoverable identity and MUST NOT
cause a host fallback, a deletion or a baseline reset. After an application restart a remote workspace SHALL
recover from its persisted volume and runtime ids without copying from the source again. Concurrent panes
SHALL share one workspace lease and one policy generation. Orphan discovery SHALL use workspace ids,
ownership and receipts, not slug or container-name guesses.

#### Scenario: Compute recreation loses no private data

- **WHEN** the container of a running private workspace is destroyed and recreated
- **THEN** the repository, baseline and journal are intact and the new container mounts the same repository

#### Scenario: Application kill and restart

- **WHEN** the application is killed mid-session and restarted
- **THEN** the workspace is found by its persisted identity in its prior state, with no new capture

#### Scenario: Runtime loss is not a fallback

- **WHEN** the runtime or its gateway becomes unavailable
- **THEN** the workspace is `runtime-lost`, no process is started on the host, and nothing is deleted or reset

#### Scenario: Volume loss is visible

- **WHEN** the authoritative volume disappears
- **THEN** the workspace reports the loss and keeps its identity and receipts, and no silent re-capture replaces it

### Requirement: Discard is a separate, custody-preserving action

Discarding a private workspace SHALL be an explicit user action that is distinct from stopping, syncing or
recreating compute, and SHALL require exclusive mutation admission. Credentials issued for the workspace SHALL
expire or be revoked on runtime stop or discard independently of file retention, and secrets MUST NOT be
stored in journals, patches or logs. Caches SHALL default to private writable caches or reviewed read-only
seeds; a shared writable cache, Nix daemon access or an engine or control socket MUST be a separate explicit
grant and never an implicit consequence of a toolchain preset.

#### Scenario: Sync does not discard

- **WHEN** a sync completes successfully
- **THEN** the private copy remains available until the user explicitly discards it

#### Scenario: Credentials end with the runtime

- **WHEN** the runtime of a private workspace stops
- **THEN** its task credentials are revoked while the repository and journal remain

### Requirement: Existing workspaces convert only by explicit action

thegn MUST NOT convert an existing worktree to `private-copy` implicitly. Existing live workspaces SHALL be
labelled with their exposure. Conversion SHALL be an explicit action that preserves dirty, staged, untracked,
binary and mode content through capture, SHALL keep the old tree untouched until the user verifies the capture
and confirms, and MUST NOT delete the old tree on its own.

#### Scenario: Conversion preserves content and the old tree

- **WHEN** the user converts a worktree with dirty, staged, untracked, binary and mode-changed files
- **THEN** the private copy contains all of them and the old tree is unchanged

#### Scenario: Nothing converts on upgrade

- **WHEN** thegn is upgraded with existing worktrees
- **THEN** their modes are unchanged and they are labelled with their exposure
