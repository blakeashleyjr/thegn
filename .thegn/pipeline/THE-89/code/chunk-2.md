# Chunk 2: Daemon integration + attention plumbing (thegn-host)

## Scope

Wires Chunk 1's pure classification into the daemon session actor
and the hydration thread's attention-scoring pass. The daemon classifies
each completed history line against the configured signatures and
publishes the error state. The hydration thread reads that state and
feeds it into `AttentionInputs`.

## Files touched (exact paths)

| File                                        | Action                                                                 |
| ------------------------------------------- | ---------------------------------------------------------------------- |
| `crates/thegn-host/src/daemon/session.rs`   | edit: add `AgentErrorState` to `SessionActor`, classify in `on_output` |
| `crates/thegn-host/src/attention_status.rs` | edit: read daemon error state into `AttentionInputs`                   |
| `crates/thegn-host/src/hydrate_feed.rs`     | edit: (if needed) publish error state to hydration pass                |

## Approach

### Step 1: `SessionActor` gains error state (`crates/thegn-host/src/daemon/session.rs`)

Add two fields:

```rust
// In the struct definition, after `osc_signals`:
error_state: thegn_core::agent_error::AgentErrorState,
error_signatures: thegn_core::agent_error::AgentErrorSignatures,
```

Initialize from config in `SessionActor::new`:

```rust
error_state: AgentErrorState::default(),
error_signatures: AgentErrorSignatures {
    signatures: cfg.notifications.agent_error_signatures.clone(),
},
```

In `on_output`, after line history is updated and before state
publication:

```rust
// Classify each completed line. Only match when signatures are
// configured (the defaults are always present, so this is always
// active unless an operator explicitly empties the list).
let pushed = self.history.total_pushed() - pushed_before;
if pushed > 0 && !self.error_signatures.is_empty() {
    let len = self.history.len();
    let start = len.saturating_sub(pushed as usize);
    let mut hit_any = false;
    for i in start..len {
        if let Some(line) = self.history.get(i) {
            if thegn_core::agent_error::classify_error_line(
                line,
                &self.error_signatures,
            ).is_some()
            {
                self.error_state.note_error(line);
                hit_any = true;
                break; // one match per chunk is enough
            }
        }
    }
    if !hit_any {
        // A chunk with no error line clears the state — the agent
        // resumed normal output.
        self.error_state.clear_on_resume();
    }
}
```

**Clear on resume:** The key invariant: a chunk without any
error-signature match clears the error state. This means a transient
tool-call failure followed by normal output (spinner, progress, text)
correctly clears the glyph.

**Publish state:** The error state is published alongside the activity
state, so the compositor's hydration thread can read it. Use the
existing `SessionActivityEvent` structure — add a boolean field
`error_active` to it. The field is set before `publish_state` sends.

```rust
// In publish_state:
let ev = SessionActivityEvent {
    session: self.meta.id.clone(),
    state: state_str(state).to_string(),
    activity: activity_str(self.activity.kind()).to_string(),
    error_active: self.error_state.error_active,
    // ... existing fields
};
```

### Step 2: Control API schema (`crates/thegn-svc/src/control/mod.rs`)

Add `error_active: bool` to `SessionActivityEvent`:

```rust
pub struct SessionActivityEvent {
    // ... existing fields ...
    /// Whether the agent has emitted a harness failure banner that has
    /// not yet been cleared by resumed normal output.
    #[serde(default)]
    pub error_active: bool,
}
```

This travels over the wire so a compositor connected remotely can also
render the error glyph.

### Step 3: Hydration thread reads error state (`crates/thegn-host/src/attention_status.rs`)

In `collect_attention`, for each worktree, read the daemon session
states:

```rust
// For each worktree: check whether any of its daemon sessions
// have error_active == true.
let agent_error_active = session_activity_events
    .iter()
    .any(|ev| ev.error_active && ev.worktree == Some(wt_path.clone()));
```

Thread this into `AttentionInputs`:

```rust
AttentionInputs {
    // ... existing fields ...
    agent_error_active,
}
```

The daemon session activity events are already available in the
hydration pass (the compositor subscribes to the daemon event feed).
If they are NOT yet plumbed through `collect_attention`, add them:

- The `DaemonSessions` refresh kind already carries session states
- The `statusbar_badges` pass already reads the event feed
- Add a `HashMap<String, SessionActivityEvent>` to
  `collect_attention`'s inputs (or read from the existing
  `daemon_session_events` cache)

### Step 4: Help page + config docs

Add to `docs/help/notifications.md`:

```markdown
### `agent_error_signatures`

A list of case-insensitive substrings. When any line of live
agent output contains one of these, the worktree's error glyph
lights up. The glyph clears automatically as soon as the agent
produces output with no matching line (e.g. it resumes working).

Defaults:

| Signature            | What it catches                |
| -------------------- | ------------------------------ |
| `weekly limit`       | Weekly usage cap               |
| `rate limit`         | Rate-limited API response      |
| `usage limit`        | Generic usage cap              |
| `limit reached`      | Catch-all limit message        |
| `quota exceeded`     | Cloud quota exhausted          |
| `connection error.`  | Network failure (note the `.`) |
| `connection refused` | TCP RST                        |
| `network error`      | Generic network fault          |
| …                    | …                              |

Set this to `[]` to disable text-based error detection entirely.
```

And a config-reference entry under `[notifications]`.

## Tests (run these)

```sh
# Unit tests:
cargo test -p thegn-host -- session::error_state_lifecycle
cargo test -p thegn-host -- attention_status::agent_error_active

# Full crate check:
just quick thegn-host

# Help-page ratchet:
just help-ratchet-update  # regenerate if new notification kind added
```

### Required test cases

1. A chunk with `"Weekly limit reached"` → `error_active` = true
2. A subsequent chunk with normal output (no matches) →
   `error_active` = false (cleared on resume)
3. A chunk with only tool-call noise (`"Error: x"`, `"● Fetch"`) →
   `error_active` stays false
4. `error_active` propagates through `AttentionInputs` →
   `AttentionTier::Failure`
5. Daemon session with `error_active` → attention-score reflects it
6. No regression: existing attention tests still pass

## Done criteria

- [ ] `cargo test -p thegn-host -- session` passes (including new tests)
- [ ] `cargo test -p thegn-host -- attention_status` passes
- [ ] `just quick thegn-host` passes (clippy)
- [ ] Manual: run thegn with an agent, trigger an error banner,
      verify glyph lights and clears
- [ ] Help-page ratchet up to date (`just help-ratchet-update` if
      needed)
- [ ] Commit subject: `feat(thegn-host): daemon-classified agent errors drive attention (THE-89)`

## Overlap / dependencies

- **Depends on:** Chunk 1 (`agent_error` module in thegn-core)
- **Blocked by:** Chunk 1 MUST be committed first
- **Blocks:** nothing
- **Parallelism:** serial after Chunk 1 (file-disjoint from Chunk 1,
  but semantically depends on the types + config key it defines)
