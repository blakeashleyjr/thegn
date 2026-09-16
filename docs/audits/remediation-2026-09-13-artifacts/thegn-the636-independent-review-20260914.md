# THE636 independent fixture-isolation review

Reviewed the integration handler test relocation and new
`crates/thegn-host/src/platform/startup_watchdog_tests.rs` read-only. The handler
path attribute preserves the existing private tests module and exact test names.
Only the real POSIX spawn case is Unix-gated; the four no-spawn policy tests
remain cross-platform. Production watchdog behavior is unchanged.

An owned existing worktree ensures group_cwd selects the intended real resolver
path. Private state/config/runtime and empty env definitions plus disabled
sandbox/backend None/placement/daemon remove the former ambient Podman route.
Panes::new has no daemon configuration. The real spawned pane must have no
provider session, the old leaf must be replaced, and a second tick preserves
pane identity/status without another swap.

The owned executable SHELL adapter validates the production -lc invocation and
executes its unchanged clean-shell command via non-login /bin/sh. Private empty
ENV/BASH_ENV and existing EnvVarGuard avoid personal startup files while HOME
remains untouched. The existing clean-shell command is POSIX and Windows shell()
ignores SHELL, so claiming this as native Windows coverage would be incorrect.

TempDir is retained after pane fields for unwind cleanup. The real-spawn test
explicitly clears owned panes before TempDir::close().expect, making normal-path
cleanup failure visible. No broad cleanup, container operations, source edits,
Cargo, or process signals were performed during this independent review.

Source verdict: approve scoped test repair. Rebuilt native test execution and
outer private-runner cleanup remain required gates; a Rust PASS followed by
runner cleanup failure is not an overall pass.
