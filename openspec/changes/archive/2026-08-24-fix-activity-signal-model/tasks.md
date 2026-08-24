# Tasks

## 1. The decision, made pure and testable

- [x] 1.1 Lift the per-worktree transition out of `activity::poll` into a new
      pure `activity_step` module (`Marks` in, `Marks` out, clock injected), so
      the edges are testable without the filesystem, `/proc`, or a CPU-burner
      subprocess — **unit tests** for every edge.
- [x] 1.2 Make quiet **confirmed**: `quiet_since` becomes the start of the quiet
      streak (mirroring `busy_since` on the resume edge), so arming needs two
      consecutive non-busy observations plus the grace at any cadence. This also
      removes the `last_active_at.unwrap_or(0.0)` trap, where an entry with no
      stamp measured its idleness from the epoch — **unit tests**.
- [x] 1.3 Add three-valued `Agentness` (`Unknown`/`Present`/`Absent`) and gate
      the needs-attention state on it, including self-healing an inherited red
      dot. `Unknown` keeps the pre-gate behaviour — **unit tests**.

## 2. Honest CPU accounting

- [x] 2.1 `scan_proc` returns `pid -> jiffies` per worktree instead of a sum;
      `cpu_jiffies_by_path` (the bridge's wire format) sums it, unchanged.
- [x] 2.2 Add `cpu_delta`: only processes present in both samples contribute; a
      first sighting baselines at zero; a vanished process drops out; a counter
      that went backwards re-baselines as pid reuse — **unit tests**.
- [x] 2.3 Persist the per-pid baselines on the entry, defaulting empty so an
      older snapshot re-baselines on the next poll.

## 3. Threading the signals

- [x] 3.1 Replace the growing positional `poll_and_save_*` signature with a
      `PollInputs` struct (extra jiffies, output hints, agent evidence, config).
- [x] 3.2 Add the shared `activity::is_real_agent` predicate and use it at all
      three sites that had copy-pasted the `"shell"`/`"local"` sentinel list.
- [x] 3.3 Build the agent-evidence map in `hydrate` before the poll, reusing the
      `db.worktrees()` read that was being taken twice per hydration.

## 4. Seeing the agent that is actually there

- [x] 4.1 Add `PtyPane::foreground_program`: the live foreground program,
      descending past nested shells and runtime wrappers, `None` for an idle
      prompt. Leave `foreground_command`'s single-hop semantics alone — a nested
      shell is still not worth offering to relaunch.
- [x] 4.2 Replace the negative pane filter with a positive one: a shell pane
      counts when its live foreground program is a recognized agent (configured
      `[[agents]]`, the worktree's bound agent, or `[activity] agent_programs`),
      so `htop`/`watch`/a dev server can never qualify — **unit tests**.
- [x] 4.3 Publish the worktrees observed running an agent and union them into
      the agent-evidence map, so a hand-started agent's finished alert is not
      swallowed by the very gate that fixes the bare-terminal bug.

## 5. Solicited repaints

- [x] 5.1 `PtyPane::resize` marks output solicited after a real geometry change
      (not a same-size no-op) — **unit test**.
- [x] 5.2 Add `PaneEvent::Reattached` and mark the pane solicited before the
      replayed scrollback is fed.

## 6. Finished versus blocked

- [x] 6.1 Add the `activity_done` palette slot (amber) beside `activity_waiting`
      (red, now meaning blocked-on-you), with config overrides — **unit tests**.
- [x] 6.2 Colour the dot from the row's attention tier: `Blocked` keeps red,
      anything else awaiting is amber. Seen-versus-unread stays the
      filled/hollow glyph axis — **unit test**.

## 7. Configuration and docs

- [x] 7.1 New `[activity]` config section in its own module, defaults preserving
      documented behaviour, clamping accessors — **unit tests**.
- [x] 7.2 Document `[activity]` and `activity_done` in `config.toml.example`.
- [x] 7.3 Add the activity-dot legend to `docs/help/sidebar.md` — the dots had
      five states, two colours, and no user-facing documentation at all.
- [x] 7.4 Validate: `fmt-check`, `lint`, `doc-check`, `test` (4912 passed),
      `coverage` (core ≥95%), `smoke`, `openspec validate --all --strict` (85
      passed). `just e2e` is independently broken and was excluded; note that it
      would not have covered this change anyway — `THEGN_E2E=1` disables the
      activity poll outright (`hydrate.rs`), so the dots have never had any
      end-to-end coverage. Pinning the FSM deterministically instead of bypassing
      it is left to a follow-up.

## 8. Regression coverage

- [x] 8.1 End-to-end test against the real `/proc` scanner: a bare terminal
      worktree with shell churn and then a real CPU burner must show working and
      settle to no dot, never a needs-attention state. Verified to fail when the
      agent gate is removed.
- [x] 8.2 Rewrite the three existing FSM tests that encoded the one-window flip;
      re-point the hint-boundary test at the streak marks it actually cares
      about, and clear `quiet_since` in its seed helper (it was passing partly on
      a stale streak left by the previous case).
