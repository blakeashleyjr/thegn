# Chunk 2 — Amend ARCHITECTURE.md §2 + event-loop spec (docs)

**Depends on:** chunk 1 (SERIAL — run after it; this chunk's text describes the
off-loop arrangement chunk 1 lands; committing it before the code would make the
docs claim something the tree doesn't do yet). **Overlap:** none — chunk 2 owns
exactly the two files below.

Design: `.thegn/pipeline/THE-78/architect/design.md` §4.6.

## Files touched (exact paths)

1. `docs/ARCHITECTURE.md`
2. `openspec/specs/event-loop/spec.md`

## Approach

### 1. `docs/ARCHITECTURE.md` §2 — replace the false sentence with the true contract

Current text (end of §2, after the 8 ms batching paragraph):

> Never put blocking I/O on the loop — including at startup: anything before the
> first frame that can block (a D-Bus probe, a network call) runs on a thread
> under a cap.

Replace that sentence with (keep the surrounding paragraphs intact):

> Never put blocking I/O on the loop — and the launch path before the first
> frame runs no synchronous subprocess I/O either. The two startup git jobs —
> the main-checkout heal (`startup_heal::spawn`, over the launch dir, each
> session worktree group and the canonical checkout) and the merge-sweep's repo
> root resolve — run on named `Background`-QoS threads. The heal's completion is
> a bounded barrier (`startup_heal::HealGate`, `BARRIER_TIMEOUT_MS`) that the
> first git-reading consumer (the initial model hydration) awaits, so a stray
> `core.worktree` can never poison a hydration pass; a healed checkout pulses
> one `RefreshKind::Model` + waker. The remaining sanctioned on-loop subprocess
> sites are interactive and post-frame (`git init` on explicit user confirm,
> documented at the site) or not the loop at all (`src/cmd/` CLI verbs, work
> already inside `spawn_blocking`/threads) — the host `clippy.toml`
> `disallowed-methods` gate plus local `#[expect]`s with reasons is the
> enforceable form of this rule.

Keep the existing **Gate:** line of §2 (idle_poll unit tests, lint's single
timed-`poll_input` assertion, render_plan tests) and append:
`; thegn-host clippy.toml disallowed-methods (blocking child waits) with local expects at sanctioned off-loop sites`.

### 2. `openspec/specs/event-loop/spec.md` — the behavioral contract

Under `### Requirement: No blocking I/O on the loop`, extend the SHALL to cover
the launch path, and add one scenario. Modified requirement text:

> Blocking I/O — git, DB, or subprocess calls — SHALL NOT run on the event loop,
> and the launch path before the first frame MUST NOT run synchronous subprocess
> calls either: they run off-thread and hand results back over a channel.

Keep the existing "Expensive setup runs off-thread" scenario; add:

> #### Scenario: Startup heal runs off-loop behind a bounded barrier
>
> - **WHEN** thegn launches, the startup git heal (stray `core.worktree` strip +
>   stale main-checkout resync over the launch dir, session worktree groups and
>   the canonical checkout) runs on a `Background`-QoS thread while the loop
>   proceeds to the first frame
> - **THEN** the first git-reading consumer (the initial model hydration) awaits
>   the heal's completion bounded by `startup_heal::BARRIER_TIMEOUT_MS`, and a
>   heal that changed anything delivers one `RefreshKind::Model` + TerminalWaker
>   pulse so the UI converges without ever blocking the loop

## Tests

Docs only — no code. Run the repo's doc-shape gates that are cheap and scoped:

```sh
just openspec-validate   # strict validation of specs/ (runs in just ci; cheap standalone)
```

(If `openspec` requires the dev shell, `nix develop -c just openspec-validate`.
Do NOT run `just ci` / `just lint` here — pre-push covers them.)

## Done criteria

- [ ] §2 contains no sentence claiming synchronous startup blocking I/O is
      sanctioned; the barrier + Model-pulse contract is stated with the module
      names above (grep `startup_heal::HealGate` in ARCHITECTURE.md → ≥1 hit).
- [ ] `openspec/specs/event-loop/spec.md` "No blocking I/O on the loop"
      requirement covers the pre-first-frame path and carries the new scenario
      (grep `Startup heal runs off-loop behind a bounded barrier` → 1 hit).
- [ ] `just openspec-validate` green.
- [ ] Chunk 1 is already committed on the branch (the docs describe landed code).
- [ ] **Exact commit subject (single commit):**
      `docs(the-78): §2 + event-loop spec state the off-loop startup heal`
