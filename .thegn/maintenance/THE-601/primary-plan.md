# Primary review + greenlight — THE-601

Reviewing row 624. **APPROVED. The two questions you escalated are decided
below; steps 2–7 stand as written.**

Your evidence is right, including the distinction that matters:
**THE-589 caps only later retained diagnostics, not this initial allocation.**
Conflating those would have produced a fix that changed nothing. You also noted
THE-210's workspace safety is landed and must be preserved — it is, and it is
recent, so do not refactor around it.

## DECISION 1 — timeout keys, names and defaults

Put them under `[merge_queue]`, beside the existing gate keys (`gate_command`,
`gate_reuse_worktree`, `gate_target_dir`) — the gate's configuration already lives
there and a second home would be worse than a long name:

- `gate_timeout_secs`
- `gate_setup_timeout_secs`

**Default both to `0`, meaning disabled**, and document that plainly. Reason: the
configured gate on this machine is a full workspace test run, which legitimately
takes tens of minutes, and a default deadline short enough to be useful would
fail good branches. An operator who wants a deadline sets one. A silent default
that turns a slow build into a red gate would be a false accusation, which is the
failure mode this whole area is careful about.

Note `0`-as-disabled has a precedent trap in this repo: `merged_ttl_secs = 0`
means _never sweep_, which silently made a smoke helper vacuous. So **document
`0` at the key and make the disabled case explicit in a test**, rather than
leaving it to be inferred.

Because these are new `section.key` pairs, they trip **three** ratchets — config
example coverage, env overlay, strict config — exactly as your step 3 says. Good
that you already listed them.

## DECISION 2 — a caller cancellation token is OUT of scope

No current caller needs one. The stated failure is a gate that never completes
because a grandchild holds a pipe; a deadline plus process-group teardown and
pipe settlement covers that entirely. A cancellation token would thread a new
seam through fold, land and the queue for no present benefit. If you find a
concrete caller that cannot be served without one, report it as a follow-up
finding rather than adding it here.

## Confirmed, and the line that must not move

**Exit classification is sacred.** Setup failure, timeout, transport failure and
reap failure are `GateVerdict::Error` — infrastructure holds. Only a completed
gate exit code may reach `classify_exit`. A timeout must never present as a red
gate, and unknown child ownership must prevent cleanup and reuse rather than
assuming the lease is free.

Also: this crate **bans blocking child waits** outside sanctioned off-loop sites.
Read `crates/thegn-host/clippy.toml` before adding a subprocess call and follow
the existing `#[expect(clippy::disallowed_methods)]`-with-a-reason pattern. And be
honest in the comments about what cannot be cancelled — a process blocked in
uninterruptible I/O will not die on TERM, and the code should say so rather than
imply a guarantee.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Every lane in the previous chain shipped something that did not
build and the primary caught each — that division of labour is expected, an
optimistic report is not.
