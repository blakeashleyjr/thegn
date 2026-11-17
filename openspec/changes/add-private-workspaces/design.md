# Design

Source of truth: `docs/audits/sandbox-security-2026-10-13/design.md` (sections "Private-copy ownership and
initial capture", "File and patch transfer", "Explicit sync and conflicts", "Lifetime and recovery") and the
THE-791 alignment decisions T1 to T4, T7 and T11. This change restates them as requirements; where the audit
says "proposed", the requirement says SHALL.

No render damage channel, event-loop wake path or per-frame work is touched. Capture, export, review and sync
run off the event loop in a broker worker that pulses the `TerminalWaker` when it finishes; none of it blocks
on the loop or before the first frame. The `private_workspaces` table is a cache of identity and state; the
private repository, the baseline, the journal and the receipts are the durable truth and live on disk outside
the workload.

## Modes are a workspace choice, not a backend choice

`workspace.mode` has three values: `private-copy`, `git-worktree` (today's linked worktree, whose common Git
metadata stays shared) and `live-mount` (an existing host tree mounted at its path). Mode and execution
backend are independent: a private copy run as a host process still lacks confinement, and a container over a
live mount still writes host files. Every launch shows workspace mode, actual boundary, home exposure, shared
writable mounts and effective egress separately. Changing mode creates a new workspace generation; it never
alters a running pane or deletes the previous copy.

Defaults follow the launch class (see `add-launch-admission`). `InteractiveOwner` keeps `git-worktree` and the
`standard` profile (owner decision 2026-11-05; `standard` is an accident guard, not a boundary). The untrusted
classes default to `private-copy` once their route is admitted and a qualified backend exists. The type is
`WorkspaceMode` (`workspace_mode.rs`, WS00-P1-14). The feature gate is `PrivateCopy`; nothing here is on by
default for interactive panes.

## Storage and identity

A workspace gets a random UUID under a private durable data root scoped by user and repository identity.
Storage is split in two. The runtime-writable repository is the only thing mounted into the workload. The
broker-owned metadata (baseline manifest and blobs, transaction journals, review artifacts, lease records and
audit receipts) is never mounted. The parent directory, the host checkout, application state, control sockets,
the source common Git directory and neighbouring copies are never mounted either. Local permission bits do not
protect metadata from an unrestricted same-UID host process; the filesystem boundary comes from admission, and
a stronger same-user threat model needs a separate broker identity or a VM.

Authoritative storage is a host directory, a named volume or a verified remote volume. A container writable
layer or a tmpfs is ineligible, which is the manifest column `StorageSupport` (WS02-P5-07): private-copy
admission refuses `Ephemeral`.

## Capture

Capture is an explicit snapshot of the selected host worktree: dirty tracked files, staged and unstaged
content as currently visible, and non-ignored untracked files. The host index is recorded for conflict
diagnostics and left unchanged. Reads verify identity and content twice (or use a filesystem snapshot);
changed input aborts or retries rather than declaring a baseline. Source Git configuration is executable
input: capture and history transport disable fsmonitor, hooks, filters, external diff and credential helpers,
use a broker-owned Git configuration and confine parser operations. An ordinary host checkout or filter is
never run to build the copy.

The private repository is created fresh. Only the chosen base commit and explicitly selected reachable
history are imported, by bundle into newly owned objects (reusing the bundle-in code of THE-2021), or a
reviewed history-free initial commit is used. The linked `.git` pointer, `commondir`, source `config`, hooks,
remotes, credentials, worktree registrations, index, `objects/info/alternates` and unrelated refs are never
copied. Shared-object clones and hardlinks are never used; reflinks only where the filesystem guarantees
independent writes. Source remotes are removed; the private identity is minimal, with no push URL and an empty
hooks directory. Before admission, and again at every reuse, confinement is verified: `.git` is a real private
directory, the common dir, object directory and refs resolve inside it, and there are no external alternates,
symlinked metadata or shared object inodes. Historical blobs can contain secrets; private copy does not
sanitise repository content.

Tracked files are always kept. Initially ignored untracked files are excluded by default, their count is
shown, and including one is an explicit user-owned choice; afterwards they are ordinary baseline entries.
Relative symlinks are preserved without dereferencing; escaping links are rejected on import. Submodules and
Git LFS need independent support, so an affected launch is refused or needs an explicit exclusion.

The file baseline is immutable and held outside the workload. An entry is the relative path bytes, file type,
executable mode, length, content hash or symlink target, and a tracked, untracked or imported class. The
baseline also records the source checkout identity, HEAD or base OID, host index fingerprint, capture
generation and ignore policy. Content hashes and owned blobs, not mtime or branch names, decide conflicts.
Sandbox commits are convenient private history; sync uses the file baseline even if the agent rewrites or
resets them.

## Export, review and sync

Export runs a trusted, bounded reader, never a command proposed by the agent. Remote exports land in
quarantine first. Paths, types and hashes are validated before review. The host never executes Git from the
sandbox repository with its configuration, hooks, filters, textconv, external diff or credential helpers. The
review artifact is a binary-capable patch built from broker-owned baseline and candidate blobs in a sanitised
temporary object store, a path, type, mode and hash manifest, and content blobs for binary additions. It has
no commit graph, refs, hooks or sandbox configuration, and terminal escapes in names and content are
neutralised when rendered.

Sync uses three snapshots: baseline `B`, candidate sandbox `S` and current host `H`. Touched paths are those
where `S != B`; host-only changes on other paths are preserved. On each touched path `H == B` (absence, type
and mode included) is required. A new sandbox path colliding with a new host path is a conflict. A changed
ancestor type, checkout identity, branch, HEAD or host index state invalidates the reviewed transaction. On a
conflict nothing is applied, both versions are kept, conflicting paths are listed and re-capture is offered.
There is no auto-merge, force overwrite, staging, commit, cherry-pick or push. A textual `git apply --check`
alone is never the safety argument.

Apply order: write and fsync all candidate blobs and backups, persist a journal with preimages, desired
hashes, destination identities and ordered operations before each mutation, then per file write a
same-filesystem temporary and rename atomically, and fsync changed directories. Host editors do not honour
Thegn's lock and a filesystem has no general multi-file compare-and-swap, so the UI requires quiescent host
editing and the implementation detects races and retains unexpected bytes instead of claiming atomicity.
Recovery distinguishes old bytes, transaction-written new bytes and unexpected third-party bytes; it resumes
or rolls back only known states and halts on anything else, never overwriting a later edit. Transaction ids
and receipts make repeated sync idempotent. After success the sync baseline advances for applied paths only.

## Lifecycle

States: `capturing`, `ready`, `running`, `stopped`, `review-ready`, `syncing`, with side states
`capture-failed`, `runtime-lost`, `conflict` and `sync-recovery-needed`. The path is `capturing -> ready ->
running <-> stopped -> review-ready -> syncing -> ready`. Identity, generation, runtime and storage ids and
transitions are persisted outside the workload. A stopped or lost runtime is not a discarded workspace.
Container recreation, daemon pruning, warm-pool cleanup, dormancy and application shutdown never remove the
repository or the journal; a successful sync keeps the copy until explicit discard. Concurrent panes share a
workspace lease and one policy generation; sync and discard take exclusive mutation admission. Orphan
discovery uses workspace ids, ownership and receipts, not slug or container-name guesses. A gateway crash or
unavailable disk yields `runtime-lost` with recoverable identity and never a host fallback, deletion or
baseline reset.

Credentials expire or are revoked on runtime stop or discard independently of file retention, and secrets
never enter journals, patches or logs. Caches default to private writable caches or reviewed read-only seeds.

## Conversion

Existing workspaces are not touched by default and are labelled with their exposure (WS11-P5-01). Conversion
is explicit, preserves dirty, staged, untracked, binary and mode content through the same capture, and keeps
the old tree until the user verifies the capture and confirms.

## Alternatives rejected

- A linked clone with shared or alternate objects: shares refs, objects and hooks, so it does not isolate
  integrity. It was the earlier private-clone design.
- Cloning only HEAD: loses dirty work.
- Syncing commits or refs back: imports attacker-controlled Git metadata. Only files and patches return.
- Depending on a container writable layer or an export during shutdown for durability.
