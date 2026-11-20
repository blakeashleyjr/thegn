# Design

`app_bundle_fs.py` is the narrow filesystem boundary for `make-app.sh`. It
requires Python 3.9 or newer, available in the repository's macOS development
environment. The shell script checks for `python3` before creating or changing
the destination and prints a specific diagnostic when it is missing.

The helper resolves the selected destination, opens each canonical directory
component with `O_NOFOLLOW`, compares the opened inode with the preceding
`lstat`, and `fchdir`s to the final descriptor before re-executing the existing
generator. The generator then uses only names relative to that pinned working
directory.

Promotion reopens the staged and existing bundle directories without following
symlinks, checks the generator marker (or the accepted legacy ownership shape),
and performs `renameatx_np(RENAME_SWAP)` on Darwin or `renameat2(RENAME_EXCHANGE)`
on Linux. First installation uses the corresponding exclusive/no-replace flag.
If safe exchange is unavailable, promotion fails without changing the app
path. After exchange, the displaced generation is removed only after another
marker and `(st_dev, st_ino)` check. The verified exchange is the commit point:
if retirement of the displaced generation fails
partway through, the new complete app stays active and the old residue remains
at the staging name for manual cleanup. Recursive removal uses directory
descriptors and no-follow opens, compatible with stock Python 3.9; it does not
rely on the newer `shutil.rmtree(dir_fd=...)` API.

This closes pathname substitution and symlink-swap races at the destination
and app entry. A process with the same UID (or root) can still mutate bundle
contents in place while generation runs; the helper cannot isolate the
generator from another process with equivalent filesystem authority. It
rechecks the displaced root inode and ownership immediately after exchange
before removing it.

The Linux test seam continues to shim `uname`, the binary, and `plutil`; it
exercises the same Python filesystem helper used on Darwin.
