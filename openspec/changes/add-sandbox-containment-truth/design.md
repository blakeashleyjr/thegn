## Context

Two stores of "which sandbox" exist today, and they were being read interchangeably:

- **Intent** — the wizard's pick, persisted in `worktrees.sandbox_backend` / `terminals.
sandbox_backend`. `agent.rs` documents this explicitly: the resolved backend is _deliberately_
  not written back, because the column is an override store that drives re-resolution on later
  opens. That design is correct and worth keeping.
- **Fact** — what a launch actually entered, which only the resolved spec and the composed argv
  know.

The UI read intent and rendered it as fact. On the worktree path this mostly worked, because
`compose_spec` labels from `SandboxOutcome.backend_label` and an explicit pick refuses to fall back
(`explicit sandbox backend '…' resolved to '…'; refusing fallback`). The terminal path had no such
guard: `sandbox_wrap_shell` calls `resolve_placed`, which walks the chain to `Backend::None` when
the requested runtime is not running, and `enter_argv` for `None` emits a bare `sh -lc`. The
function returned `Some(argv)` regardless, and the caller stamped the requested name.

Underneath sits a third distinction that already exists and is unused at launch time:
`sandbox_support::BackendState` separates `NotRunning` from `NotInstalled`, and `remedy_for()`
knows the command that fixes each. The onboarding wizard renders them; the resolver folds both into
"absent" and moves on.

## Goals / Non-Goals

**Goals:**

- A pane can never claim containment it does not have.
- The guarantee is enforced by a test that cannot rot by omission when a backend is added.
- Keep intent persisted, so a pick takes effect once the runtime is running.
- Turn a dormant runtime into a decision the user makes, not one made silently for them.

**Non-Goals:**

- Verifying containment from _inside_ the pane (asking the kernel what namespace the process is
  in). Argv derivation is a large improvement over trusting the request; in-pane attestation is a
  separate, heavier design.
- Changing which backend the chain picks, or the preference order.
- Auto-starting runtimes without asking.

## Decisions

**Derive the label from the argv, not from the resolver's return value.** The resolver's answer is
a claim; the argv is what actually runs. Deriving from the argv means a bug anywhere upstream — a
resolver that returns the wrong backend, a wrap helper that forgets to check — surfaces as a
correct label plus a warning, not as a false claim. This is why the check lives at the point the
`LaunchSpec` is built, on both paths, rather than being pushed back into the resolver.

**Command position only.** Scanning the whole argv for a runtime name would be simpler and is
badly wrong in the dangerous direction: a worktree at `~/code/docker` would read as containment.
So derivation walks command words — `argv[0]`, whatever a pass-through (`sudo`, `env`, `exec`)
hands off to, the token after an end-of-flags `--`, and the first word of each command inside an
embedded script — and ignores everything in argument position. Alternatives rejected: matching only
`argv[0]` (misses `sudo -n podman` and every transport-wrapped remote), and matching anywhere
(false containment claims).

**Argv derivation is authoritative for local placements only.** A remote placement runs its
container on another machine behind a transport, and its argv can legitimately show no runtime.
There the resolver's label is kept, since the placement's own bring-up already proved the runtime
exists. This is a deliberate scope limit, not an oversight: a false `host` is the safe direction,
but it is still a wrong label, and it would fire on every provider pane.

**Exhaustiveness by compilation, not by discipline.** The gate walks a backend list that contains a
dead `match` over every variant, so adding one is a compile error in the test. A hand-maintained
list would drift the moment someone added a backend in a hurry — which is exactly the class of
omission that produced the original bug.

**Native-Windows backends are taken at their word, and it is written down.** Their isolation
happens in the spawn syscall, so the argv is a plain shell either way. Reporting `host` for them
would be a different lie. The exception is narrow, documented at the derivation site, and pinned by
its own test.

**Split the columns rather than overwrite intent.** Writing the observed backend into
`sandbox_backend` would make the chip honest and then lose the user's pick — a user who fixes their
podman machine would silently get a host shell forever after. So the fix is an added column for the
observed value, with display reading observed and re-resolution reading intent. No `user_version`
bump: `db_migrate.rs` already adds `sandbox_backend` and `env_name` with a bare `ALTER TABLE … ADD
COLUMN` (a no-op once the column exists, and merge-safe across branches), and the observed columns
follow that same additive convention.

A consequence worth stating: the tab chip no longer predicts. It used to show the backend config
_would_ resolve to before a worktree's first launch, so the chip was never empty. That prediction
was a claim rendered as fact, so it is gone — a chip that is briefly empty is honest, and it fills
in the moment a pane actually launches.

**Reuse the failover shape for dormant runtimes (pending — group 5).** `FailoverMode::Ask` already models
"stop and ask before degrading" for environments. A `[sandbox] on_dormant = ask|start|host|cancel`
knob follows that precedent instead of inventing a second policy vocabulary, and the start action
already has everything it needs: `remedy_for()` for the command and `clear_probe_cache()` for the
re-probe.

## Risks / Trade-offs

- **Users see `host` where they used to see their pick** → That is the fix, but it will read as a
  regression. The warning names the unavailable backend so the cause is visible, and the dormant
  prompt turns it into an actionable choice.
- **A remote/provider pane keeps the resolver's label** → Documented scope limit; revisit if
  transports gain a uniform way to report the runtime they reached.
- **Derivation could still miss an exotic wrapper** and report `host` for a contained pane → Safe
  direction (understates containment), and the round-trip gate covers every backend thegn itself
  builds argv for.
- **`sudo` detection is positional** — a `sudo` anywhere before `podman` implies rootful → Matches
  how `oci_prefix` builds rootful argv; the round-trip test pins both spellings.
- **Migration adds a column to a live DB** → The DB is a cache with a versioned schema; the
  migration is additive and a missing observed value falls back to reporting nothing rather than
  guessing.

## Migration Plan

Groups 1-4 are additive: two new nullable columns (no `user_version` bump, per the repo's
convention for additive columns) and behavior that only ever reports _less_ containment than
before. An existing DB gains the columns as NULL, which reads as "never launched" and displays as
nothing until the next launch records an observation. Rollback is reverting the label derivation,
which restores the old (untruthful) behavior; the columns are harmless if left behind.

## Open Questions

- Should a degraded pane be visually marked in the tab chip (e.g. `host ⚠`) rather than only
  warned about once at launch? A warning scrolls away; the containment claim persists.
- Should `thegn doctor` grow a "what is this pane actually in?" report using the same derivation?
- Does the observed value belong on the terminal/worktree row at all, or on the pane/session record
  — given it is a property of a _launch_, and a row outlives many launches?
