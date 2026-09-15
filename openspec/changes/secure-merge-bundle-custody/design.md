# Design

`merge_remote::fetch_bundle` owns a `BundleTemp` for the complete
`git fetch` call. On Unix, `tempfile` creates a random directory below the
canonical system temporary parent; `Directory::open(..., Some(path))` pins and
checks that private directory, and `Regular::create_exclusive` uses the pinned
parent with `O_NOFOLLOW|O_CREAT|O_EXCL|O_CLOEXEC|O_NONBLOCK` and mode `0600`.
The retained `Regular` receives the bundle bytes, is flushed and revalidated,
and is then passed by path to the existing target-host Git command. Drop uses
`remove_verified` and nonrecursive `remove_dir` only while the retained
identities still verify. A failed verification leaves the replacement or
foreign entry in place for diagnosis.

The supported Unix custody boundary is a sticky system temporary parent plus a
random owner-only `0700` directory created with that mode and an exclusive
owner-only `0600` leaf. Temporary-directory recursive cleanup is disabled as
soon as the directory is created; setup failures use only bounded
identity-checked/nonrecursive cleanup.
This rejects foreign ownership, shared writable directories, special files,
hardlinks and observed path replacement. It does not claim atomic cleanup
against an arbitrary same-UID process changing the path between observation and
unlink.

Windows does not route through the Unix-only `gate_path` implementation. It
uses random names and `CreateDirectoryW` with a protected owner-only DACL in
`SECURITY_ATTRIBUTES` at creation, using the current process token's user SID
in retained aligned storage. The helper reads back the exact protected ACL
and retains a no-follow directory handle before creating the leaf. The
fixed leaf uses `create_new(true)` and
`FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE`. Cleanup compares
retained and no-follow-opened handles through stable
`GetFileInformationByHandle` volume/index identity before removing the leaf
and then the empty directory. Reparse points are refused by the existing
platform helpers. Windows native proof is a remaining delivery gate.

Tests use only private `TempDir` fixtures. They cover exclusive creation,
observed replacement preservation, the actual bundle-to-fetch seam, and two
child processes contending for one exact leaf. The child protocol reserves
private result directories before spawn, uses null stdio and a finite marker
barrier and bounded owned-child cleanup. Observation loss retains the child
and its root in the existing fixed custody slots rather than asserting it was
reaped. Successful fixtures remove only entries they own.
No ambient PID/name paths, provider, live repository, or unbounded pipe is used.
