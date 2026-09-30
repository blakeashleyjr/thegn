# Secure cross-host merge bundle custody

THE-222 repairs the local temporary-file boundary used while fetching a
cross-host merge-queue bundle. The prior path was predictable, opened through
the ambient filesystem, and removed by name after `git fetch`, so a FIFO,
symlink, clobber, or replacement could redirect the bundle write or cleanup.

Create an unpredictable private temporary directory, retain its physical
identity, create one owner-only bundle leaf with exclusive creation, retain the
opened handle through the fetch, and remove only an identity that still matches
the retained custody. Reuse the existing Unix descriptor-relative
`platform::gate_path` checks. Windows keeps its existing fetch support through a
local `CREATE_NEW`/delete-sharing seam, an explicit protected owner-only DACL,
and `open_*_nofollow` reparse checks; Windows native execution remains
explicitly unverified until a Windows runner is available.

This change covers the merge-remote bundle leaf and its narrow platform seam.
It does not change remote synchronization, Git streaming (THE-223), merge
folding, provider behavior, or the existing general `gate_path` policy. An
identity check followed by unlink/rmdir remains observational: it detects an
observed replacement and preserves it, but cannot provide an atomic lease
against an arbitrary same-UID writer.
