# Primary review — THE-455 revision 2 (reviewing row 673)

You put the no-follow open in `thegn-core`'s `fsperm.rs` as decided, which was
right. It does not compile, and the reason is a detail of that file I should have
checked before pointing you at it.

## The failure

```
crates/thegn-core/src/fsperm.rs:26,46,378  error[E0433]:
  cannot find module or crate `libc` in this scope
```

## Use `nix`, not `libc` — `libc` is macOS-only in this crate

`crates/thegn-core/Cargo.toml` gates its platform dependencies like this:

```toml
[target.'cfg(unix)'.dependencies]
nix.workspace = true

[target.'cfg(target_os = "macos")'.dependencies]
libc.workspace = true          # for the libproc activity scanner
```

So on Linux, core has **`nix`** and does **not** have `libc`. `libc` is there only
for the macOS activity scanner's `proc_listallpids` path. That is why the same call
would compile on a Mac and fails here.

**The workspace already enables `nix`'s `fs` feature**, which is exactly what this
needs:

- `nix::fcntl::{open, OFlag}` with `OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK |
OFlag::O_RDONLY` — no new dependency, no new feature, no workspace manifest
  change.
- `nix::sys::stat::fstat` on the returned descriptor to confirm a regular file
  **from the descriptor**, which is the check that closes the TOCTOU.

`thegn-host/src/platform/unix.rs` already uses `nix` this way, so this also matches
the house style rather than introducing a second way to make a syscall.

Do **not** add `libc` to core's unix dependencies to make the current code work. It
would grow the closure THE-669 is actively cutting, and it would be the second
platform-syscall crate in one file.

## Everything else from the previous two reviews stands

`fsperm.rs` remains the right home (both crates reach it, it is already pinned on
`test/platform-cfg-core-ratchet.txt`, so a per-OS branch inside it needs no new
allowlist entry). And the rest of the plan is unchanged: the directory-entry budget
strictly larger than the eligible-file cap; filter → stable sort → cap; `.ics`
symlinks refused; aggregate byte and file bounds enforced before each next read;
whole-account typed refusal rather than silent truncation; directory/metadata/UTF-8
failures as account-level errors; cooperative deadline checks without an owned
worker seam; deterministic-selection test with injectable ordering; bounds asserted
against the named constants.

`hydrate_calendar.rs`: **THE-459 has landed** (`4babb090`) and removed `day_ms`
there, so rebase onto current main and keep your diff in that file minimal.

## Validation

`nix develop --command cargo nextest run -p thegn-core -p thegn-svc` — the **whole**
crates, not a filter. Four gate failures this week were tests a `-E` filter
excluded, each covering the file the lane had just changed. **The pipeline sandbox
mounts `/nix/store` read-only, so this usually fails outright** — say exactly that
and stop if it does; a supervisor now runs the crate-wide verification centrally.

Never report a verdict for code you could not compile; state what you could not run.
