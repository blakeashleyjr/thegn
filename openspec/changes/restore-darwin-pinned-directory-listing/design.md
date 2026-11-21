# Design: Darwin pinned-directory listing

Use `openat(held_fd, ".", O_DIRECTORY | O_CLOEXEC)` for each listing, then
enumerate the returned descriptor with a directory stream. Keep the owned
descriptor in an RAII boundary until `fdopendir` succeeds so a failed
conversion closes it; after success the stream owner closes it with `closedir`.
This avoids the Darwin `/dev/fd` path behavior and ensures each enumeration
gets a new open file description and read offset. The held descriptor remains
the authority; no path lookup or symlink following is introduced. Stop at the
first `readdir` error, preserving prior names for best-effort cleanup and
returning the error from strict listings. Use the same helper for `PrivateDir`
and `RootedDir`. Preserve the non-Unix path implementation.
