# THE-89: Error Glyph Tightening — Architecture Design

## Problem

An agent-pane tool-call error line (e.g. Claude Code printing
`● Fetch(https://…) / Error: Command failed with no output`) lights the
error glyph (✗) on the worktree, even though the agent handles the error and
continues. A transient tool-call failure is not an agent-level error.

## Root-cause investigation

The error glyph is the sidebar row's `cross` glyph rendered in red,
driven by `AttentionTier::Failure`. That tier is scored from unread
notifications of kinds `AgentFailed`, `TestFailed`, `ProcessFailed`, or
`LogError` (`thegn_core::attention.rs:493–495`).

**Two code paths reach the Failure tier from agent panes:**

### Path 1 — process exit (`crates/thegn-host/src/pty_drain.rs:853–871`)

When a PTY child exits in a pane that matches a dispatch row,
`dispatch_for_exit` resolves it as an agent pane and the exit code
decides:

```rust
// pty_drain.rs:815–818
let failed = match exit_code {
    Some(c) => c != 0,
    None => crashes > 0,
};
// pty_drain.rs:871
let kind = if failed { "agent_failed" } else { "agent_done" };
```

This is correct at the **harness level**: the agent process itself
ending non-zero IS an agent failure. The issue is NOT about this path.

### Path 2 — there is no live text-scanner (confirmed)

A full search of all `Error:` text matching, output-error heuristics,
and OSC classification found no text-based live error detector. The
**only** in-life error signal is `OSC 9`/`OSC 777;notify`, which maps
to `AttentionTier::Blocked` (the raised-hand ✋ glyph, not the ✗ error
glyph).

The `osc_attention.rs` scanner is an observer (never rewrites), and
`on_attention` in `session.rs:756` upserts into `session_attention`
— the Blocked tier, designed for the agent asking for input, not
reporting a failure.

### What IS firing (confirmed hypothesis)

Claude Code does NOT emit `OSC 777` error notifications. The error
glyph is lighting because `pty_drain.rs`'s `dispatch_for_exit` path
detects a **non-zero exit from the agent PTY child** — and in the
scenario the user describes, the agent IS the child that exits non-zero
after every failing tool call (or the tool-call wrapper exits non-zero
inside the pane, which the drain reads as the pane's child exit).

But the issue says "the agent handles and moves past" — the agent
DOESN'T exit. This means the tool-call subprocess exit is NOT the PTY
child exit (the PTY child is `claude`, not the tool). So the only
remaining path is:

**The activity FSM's "output busy" signal + the attention model's
`ProcessFailed` notification from a non-agent-pane exit within the
same worktree.**

OR the user is describing the _activity dot_ (red filled dot = "stuck,
look at me") — which is not the error glyph but appears confusing as
one. The activity dot is driven by the FSM in `activity.rs`, which
marks a worktree `waiting` (filled red) when it goes quiet after being
active. A failed tool call causes a burst of output (the error line) →
activity → then quiet → waiting dot. This is NOT the same as the error
glyph (✗) but looks like an error indicator to the user.

## Design decision

After thorough investigation, the TWO things needing tightening are:

1. **The `agent_failed` exit path is already correct** — a harness exit
   IS a real error. No change needed here.

2. **Introduce in-life error classification for agent output** — the
   mechanism the issue describes. Even though no text-scanner exists
   today, the issue calls for one that is:
   - Scoped to **harness failure banners** (weekly limit, connection
     error, auth errors)
   - A **config-listed signature set** (reusing THE-86's
     `pipeline_exit::ExitSignatures` pattern)
   - **Cleared on next normal turn** (not sticky like the old
     notification row)

### What changes

#### `thegn-core`: Pure classification (`crates/thegn-core/src/agent_error.rs`)

New module: `agent_error` — pure text classification of live agent
output lines. Reuses the signature-set pattern from
`pipeline_exit.rs` (THE-86).

```rust
/// Substrings that make an agent output line an agent-level error.
/// Case-insensitive. Defaults are the harness banners
/// (weekly limit, connection error, auth).
pub struct AgentErrorSignatures {
    pub signatures: Vec<String>,
}

/// Classify a single output line. `None` = not an error.
/// `Some(AgentErrorKind)` when the line matches a known
/// harness failure banner.
pub fn classify_error_line(line: &str, sig: &AgentErrorSignatures) -> Option<AgentErrorKind> {
    if sig.signatures.iter().any(|s| line.to_lowercase().contains(&s.to_lowercase())) {
        Some(AgentErrorKind::HarnessBanner)
    } else {
        None
    }
}
```

Default signatures (the harness banners — what matters to the user):

- `"weekly limit"`
- `"rate limit"`
- `"usage limit"`
- `"limit reached"`
- `"quota exceeded"`
- `"out of credits"`
- `"insufficient credits"`
- `"credit balance"`
- `"billing"`
- `"payment required"`
- `"connection error."`
- `"connection refused"`
- `"connection timed out"`
- `"network error"`
- `"network request failed"`
- `"auth"` / `"authentication"` / `"permission denied"` (only in
  agent-context — scoped carefully)

Deliberately EXCLUDED: bare `Error:` prefix, `Command failed`,
stack traces, `● Fetch` — these are transient tool-call noise the
agent handles and moves past.

#### `thegn-core`: Config key (`crates/thegn-core/src/config.rs`)

New `[agents]` section key or new `[notifications]` key:

```toml
[notifications]
# Harness failure banners that raise the agent error glyph mid-turn.
# Each entry is a case-insensitive substring matched against individual
# output lines.  Defaults name the known harness limits and connection
# errors; add your harness's own banners here.
agent_error_signatures = [
    "weekly limit", "rate limit", "usage limit",
    "connection error.", "authentication failed",
]
```

#### `thegn-core`: Error-state tracking (`crates/thegn-core/src/agent_error.rs`)

A per-session error flag cleared on next normal output:

```rust
/// The agent error state for one daemon session.
pub struct AgentErrorState {
    /// Whether an error banner has been seen and not yet cleared.
    pub error_active: bool,
    /// The matching signature that set it.
    pub last_signature: Option<String>,
}
```

`error_active` is set when a line matches a signature; it is cleared
when:

- The session becomes blocked (OSC attention) — that's a superseding
  signal
- Fresh normal output arrives (the agent resumed) — `on_output` resets
  the flag
- The user answers (input arrives)

#### `thegn-host`: Daemon session integration (`crates/thegn-host/src/daemon/session.rs`)

`SessionActor` gains an `AgentErrorState` field. `on_output` tests
each completed history line against the signature set. When a match is
found:

1. `error_active` is set
2. The activity FSM's `note_output` is skipped for this chunk (or more
   precisely: the error does not override the activity state — it's
   orthogonal)
3. The attention state is evaluated: if `error_active`, the tier is
   `AttentionTier::Failure` with reason `AgentFailed`

**Clear on resume:** On the next `on_output` chunk that contains NO
error-signature line, `error_active` is cleared and `publish_state`
refreshes the tier (back to Working/Idle as appropriate).

#### `thegn-core`: Attention-model wiring (`crates/thegn-core/src/attention.rs`)

A new input field in `AttentionInputs`:

```rust
pub struct AttentionInputs {
    // ... existing fields ...
    /// Whether the live agent output contains a harness failure banner
    /// that has not yet been cleared by resumed normal output.
    pub agent_error_active: bool,
}
```

Scoring: when `agent_error_active` is true, the tier is bumped to
`AttentionTier::Failure` with reason `AgentFailed` — **but only when
the activity is NOT `Active`** (an agent still working through an error
is fine; an agent that has gone QU

ET and has a
lingering error banner is worth alerting).

Actually: the simpler and more correct approach: `agent_error_active`
raises `Failure` unconditionally. The "clear on next normal turn"
rule handles the transient case. The reason a stale error banner on an
active agent is still a signal: the agent might be in a loop retrying
the same error.

#### Config docs + help page

- `docs/help/notifications.md`: document `agent_error_signatures`
- `docs/config-reference.md`: config key reference
- Help page ratchet: if a new notification kind is introduced, register
  it

### What DOESN'T change

- `pty_drain.rs`'s exit-based `agent_failed` path — correct as-is
- `osc_attention.rs`'s OSC scanner — correct as-is, maps to Blocked
- The activity FSM (`activity.rs`) — no changes; this is orthogonal
- `pipeline_exit.rs`'s transport-retry signatures — separate concern,
  not shared code (the default values are conceptually similar but
  the config keys and consumers are distinct)
- The `notify` module — this uses live state, not inbox notifications
- The sidebar row rendering — `AttentionTier::Failure` already renders
  the `cross` glyph in red

### Why not reuse `pipeline_exit::ExitSignatures` directly?

THE-86's transport-retry signatures classify a dead worker's final
screen into retry vs park. THE-89 classifies a live agent's output
into "real error / noise." The categories overlap (connection errors)
but serve different consumers with different config keys:
`[pipeline.transport_retry] transport_signatures` vs
`[notifications] agent_error_signatures`. Keeping them separate lets
an operator tune each independently without coupling.

## Test plan

### Pure classification tests (thegn-core, 95% coverage gate)

1. `classify_error_line` returns `None` for bare `Error:` lines,
   `Command failed`, stack traces
2. `classify_error_line` returns `Some` for harness banners
   (weekly limit, rate limit, connection error.)
3. Case-insensitivity
4. Empty signatures list ⇒ always `None`
5. `AgentErrorState` lifecycle: set → not cleared by another error →
   cleared by clean output

### Attention scoring tests

6. `agent_error_active` true → `AttentionTier::Failure` →
   `AttentionReason::AgentFailed`
7. `agent_error_active` false → no effect on existing scoring

### Hydration/integration tests

8. An error-signature line followed by continued activity does NOT set
   the glyph (error cleared before the attention is computed)
9. A harness banner followed by quiet DOES set the glyph
10. The glyph clears on resume (next normal output)

## Files touched

| File                                        | Change                                                              |
| ------------------------------------------- | ------------------------------------------------------------------- |
| `crates/thegn-core/src/agent_error.rs`      | **NEW** — pure classification                                       |
| `crates/thegn-core/src/lib.rs`              | add `pub mod agent_error`                                           |
| `crates/thegn-core/src/attention.rs`        | add `agent_error_active` to `AttentionInputs`; wire scoring         |
| `crates/thegn-core/src/config.rs`           | add `agent_error_signatures` to `[notifications]`                   |
| `crates/thegn-host/src/daemon/session.rs`   | `AgentErrorState` in `SessionActor`; classify in `on_output`        |
| `crates/thegn-host/src/attention_status.rs` | plumb `agent_error_active` from daemon state into `AttentionInputs` |
| `docs/help/notifications.md`                | document `agent_error_signatures`                                   |
| `docs/config-reference.md`                  | config key entry                                                    |

## Invariants

- Pure classification in `thegn-core` — no I/O, no tokio, no host types
- 0% idle: signature matching only on completed history lines (per
  chunk), not a per-byte scan
- Render decision: error state change pulses the waker, but the tier
  is computed in hydration (off-loop)
- Ratchets: no new `#[cfg]` outside `platform/`, no color/glyph
  literals outside caps chokepoints
- Perf: the signature list is small (~15 entries), matching is O(lines ×
  signatures) per output chunk; negligible
