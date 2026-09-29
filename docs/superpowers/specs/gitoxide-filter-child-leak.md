# Upstream report draft — gix-filter leaks a filter-driver child

**Status:** drafted, not submitted. Intended for
<https://github.com/GitoxideLabs/gitoxide/issues>. See THE-701.

Submitting this is a call for a human: it posts to another project's tracker
under your name. The text below is ready to paste.

---

## Title

`gix-filter`: a long-running `process` driver's child is never waited on, leaving a zombie per use

## Body

### Summary

`gix_filter::driver` leaks its child processes in two distinct places. A caller
that diffs or checks out paths through a `filter.<name>.process` driver
accumulates one zombie per operation, for the life of the process.

Found in production: a long-running TUI that diffs worktrees against `HEAD`
accumulated **4,408 zombie `git-lfs` processes over three days**, taking the
machine's process table to 5,274 entries. System load sat at 10–13 while `top`
reported 45% idle, because processes were queuing rather than competing for CPU.
I/O pressure `full avg10` was 7.94; closing the one process dropped it to 0.00
and the process table to 906.

The global git config in question is the ordinary one git-lfs installs:

```ini
[filter "lfs"]
	clean = git-lfs clean -- %f
	smudge = git-lfs smudge -- %f
	process = git-lfs filter-process
	required = true
```

Versions: `gix` 0.84, `gix-filter` 0.31, `gix-diff` 0.64. Linux.

### Leak 1 — `State` drop does not wait

`driver::State::running` holds `process::Client`s, each owning a
`std::process::Child`. Dropping the `State` drops the clients, and **`Child` does
not wait on drop** — so each child is left unreaped.

The doc comment on that field states the opposite:

```rust
/// Note that these processes are expected to shut-down once their stdin/stdout are dropped, so nothing else
/// needs to be done to clean them up after drop.
running: HashMap<BString, process::Client>,
```

Closing the pipes makes the child **exit**; it does not make it **waited on**. An
exited, unwaited child is a zombie. The comment reads as an assurance that no
cleanup is required, which is how a caller ends up not calling `shutdown()`.

This one is at least reachable from outside: `State::shutdown(Mode::WaitForProcesses)`
does the right thing, and a caller who knows to call it is fine. Worth correcting
the comment either way, since it currently says they don't need to.

### Leak 2 — `handle_io_err` drops the client on the floor

`src/driver/apply.rs`:

```rust
pub(crate) fn handle_io_err(err: &std::io::Error, running: &mut HashMap<BString, process::Client>, process: &BStr) {
    if matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof
    ) {
        running.remove(process).expect("present or we wouldn't be here");
    }
}
```

The client is removed from `running` and dropped. It can therefore never reach
`shutdown()`, so **this path is not fixable by the caller at all** — there is no
API that reaches the client once it has been removed.

This is the path a misbehaving or mismatched filter takes, which is exactly when
the leak rate is highest.

### Reproduction

A driver that fails the long-running handshake exercises leak 2 directly. `cat`
is enough:

```sh
git config filter.tgtest.process cat
git config filter.tgtest.required true
echo 'tracked.txt filter=tgtest' > .gitattributes
```

Then run five worktree diffs through `gix`'s `diff_resource_cache(Mode::ToGit, …)`
and count children of the test process. Observed, with
`State::shutdown(Mode::WaitForProcesses)` called on drop of the resource cache:

```
5 unreaped child process(es): [("Z", "cat"), ("Z", "cat"), ("Z", "cat"), ("Z", "cat"), ("Z", "cat")]
```

The shutdown itself reports `reaped 0 process(es): []` — the state is already
empty, because `handle_io_err` took the client first.

### Suggested fix

1. `handle_io_err` should wait on the client (or hand it to something that will)
   rather than dropping it.
2. Correct the `State::running` doc comment: closing the pipes is not waiting,
   and `shutdown()` is required.

Optionally, making `State`'s `Drop` wait would close the general case for callers
who forget, at the cost of blocking in a destructor — reasonable to reject, but
then the comment above really has to change.

### Workaround in use

Prefer the one-shot program when a driver declares both, which is the shape
git-lfs installs. `gix` keeps and waits that child
(`driver.required.then_some((child, command))` in `driver::apply`), so no
persistent process is spawned and neither leak applies:

```rust
for driver in &mut cache.filter.worktree_filter.options_mut().drivers {
    if driver.required && driver.clean.is_some() {
        driver.process = None;
    }
}
```

Only safe for a `required` driver — for a non-required one the one-shot child is
discarded unwaited too, which trades one leak per _diff_ for one per _blob_.
