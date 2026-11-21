# Restore Darwin pinned-directory listing

## Why

Native macOS account-home rollback fails because `/dev/fd/<descriptor>` is not
a directory stream boundary that `std::fs::read_dir` can enumerate. The same
listing pattern appears in private-directory retention cleanup and rooted
nested-directory enumeration.

## What changes

Open each directory stream relative to the held descriptor and enumerate that
fresh stream. This keeps directory operations pinned, gives repeated listings
independent read positions, and restores nonrecursive managed-home rollback.

## Capabilities

- Update `platform-macos` pinned directory enumeration behavior.
