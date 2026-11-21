# Design: Darwin pinned-directory listing

Use `openat(held_fd, ".", O_DIRECTORY | O_CLOEXEC)` for each listing, then
enumerate the returned descriptor with a directory stream. This avoids the
Darwin `/dev/fd` path behavior and ensures each enumeration gets a new open
file description and read offset. The held descriptor remains the authority;
no path lookup or symlink following is introduced. Use the same helper for
`PrivateDir` and `RootedDir`. Preserve the non-Unix path implementation and
existing error handling at callers.
