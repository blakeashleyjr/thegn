# Git safety: how thegn avoids `.git/config` pollution

## Threat model

thegn is a multi-worktree IDE. Git worktrees **share one `.git`**: a linked
worktree's `.git` is a _file_ (`gitdir: <canonical>/.git/worktrees/<name>`), and
all worktrees write the same `<canonical>/.git/{config,objects,refs}`. Each
worktree's interactive process can run sandboxed (podman → docker → bwrap →
none), and thegn can run several agents at once.

Two failure modes have bitten us repeatedly:

1. **`core.worktree` pollution.** A git command that runs with an inherited
   `GIT_DIR`/`GIT_WORK_TREE` pointing at the shared `.git` (e.g. a process
   launched from a git hook, or an in-sandbox agent that `export GIT_DIR=…`)
   writes a stray `core.worktree` (or `user.*`) into the _shared_ `.git/config`.
   Git then silently retargets **every** operation on the main checkout at the
   wrong worktree — phantom whole-repo diffs, "the changes pane shows another
   worktree", a merge that "diverges". `git config --get/--unset` themselves
   abort (`fatal: Invalid path`) once the value points at a missing dir.
2. **Concurrent-agent races.** Multiple thegn/agent processes mutating the same
   `.git` at once (two `git worktree add`, two commits to `main`) clobber the
   shared index/refs or thrash the branch.

## The rule

**All git goes through `thegn_core::util::git_cmd` or `GitLoc`.** Both build
`git -C <dir>` with `GIT_ENV_VARS` scrubbed. Raw `Command::new("git")` is
forbidden everywhere except the one builder in `util.rs` — `just lint` enforces
this with a grep guardrail.

## The layered defense (`crates/thegn-core/src/util.rs` unless noted)

1. **Process env scrub** — `scrub_git_env()` at the top of `main()`
   (`thegn-host/src/main.rs`) removes `GIT_ENV_VARS` before any thread spawns,
   so nothing thegn launches inherits a poisoned git env.
2. **Per-invocation scrub** — `git_cmd(dir)` and the `GitLoc` local builders
   (`remote.rs` `git_command`/`git_command_env`/`sh_command`) `env_remove` the
   same vars on every git/custom-command invocation — robust even if the process
   env is dirty.
3. **Self-heal** — `heal_main_checkout_worktree()` surgically strips a stray
   `core.worktree` from a _main checkout's_ `.git/config` (text edit, because git
   can't read a config whose `core.worktree` is invalid). Runs at startup over
   cwd + session worktrees + the `--git-common-dir` parent (the canonical
   checkout, even when launched from a linked worktree), and again on each
   worktree switch (off-thread) so mid-session pollution by another agent heals.
4. **Sandbox: read-only shared `.git/config`** — `sandbox.rs` mounts
   `<git-common>/config` read-only on top of the writable `.git` (bwrap
   `--ro-bind`, OCI `:ro`, systemd `ReadOnlyPaths`). Objects/refs/index and the
   per-worktree `worktrees/<name>/config` stay writable, so commits work, but no
   sandboxed process can write the shared config. _Tradeoff:_ legitimate
   `git config`/`git remote add` from **inside a sandbox** fails by design.
5. **Sandbox: GIT\_\* env strip** — the sandbox `env_block` carries `GIT_ENV_VARS`,
   so the wrapped script `unset`s them before the shell/agent runs (bwrap/systemd
   inherit the host env; OCI already passes only a whitelist).
6. **Cross-process git lock** — `lock_git_mutations(worktree)` takes a `flock` on
   `<git-common>/thegn-git.lock` (advisory, auto-released on Drop and process
   death — no stale locks). The svc write runners (`run_w`/`run_stdin`/`run_root`
   in `thegn-svc/src/git/mod.rs`) and `worktree::{add_checked,remove}` hold it
   for the mutation, serializing concurrent agents on the same repo. Reads stay
   lock-free; remote locs lock on their own machine.

## Operational guidance (the part code can't enforce)

- **Each agent works in its own worktree.** Do **not** run multiple agents
  driving git against the canonical checkout — that is what triggers the races
  and pollution. Funnel merges to `main` through one actor.
- **Restart thegn after a rebuild.** A live _old_ binary keeps re-polluting on
  every commit; the fixes (and the startup heal) only take effect once the new
  binary is running and stale agents are stopped.
- Tests run git hermetically (`git_cmd` + `GIT_CONFIG_GLOBAL=/dev/null` + a temp
  `HOME`); never add a test that runs git in the real repo.

See the `thegn-gitconfig-pollution` history for the incident this hardens.
