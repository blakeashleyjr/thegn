# Chunk 1: Pure classification + config (thegn-core)

## Scope

This chunk introduces the pure error-classification engine and its
config key. No daemon integration, no attention-model wiring — those
are Chunk 2. The modules are self-contained and independently testable.

## Files touched (exact paths)

| File                                   | Action                                                                        |
| -------------------------------------- | ----------------------------------------------------------------------------- |
| `crates/thegn-core/src/agent_error.rs` | **CREATE**                                                                    |
| `crates/thegn-core/src/lib.rs`         | edit: add `pub mod agent_error;`                                              |
| `crates/thegn-core/src/config.rs`      | edit: add `agent_error_signatures` to `NotificationsConfig`                   |
| `crates/thegn-core/src/attention.rs`   | edit: add `agent_error_active: bool` to `AttentionInputs` + wire into `score` |

## Approach

### Step 1: `crates/thegn-core/src/agent_error.rs`

New module with three public items:

```rust
/// A matched agent-level error banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentErrorKind {
    /// Matched a harness failure banner (usage limit, connection error, auth).
    HarnessBanner,
}

/// Config-listed substrings that classify as agent-level errors.
/// Case-insensitive matching.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentErrorSignatures {
    pub signatures: Vec<String>,
}

impl AgentErrorSignatures {
    /// The shipped defaults — harness failure banners from
    /// `pipeline_exit.rs`'s DEFAULT_LIMIT_SIGNATURES and
    /// DEFAULT_TRANSPORT_SIGNATURES, filtered to the subset
    /// relevant to in-life agent output (no HTTP status
    /// codes, no low-level transport codes like `econnreset`).
    pub fn defaults() -> Self;

    /// True when no signatures are configured — matching is
    /// a no-op (never classifies anything as an error).
    pub fn is_empty(&self) -> bool;
}

/// Classify one output line.  `None` => not an agent error.
/// Matching is `line.to_lowercase().contains(sig.to_lowercase())`.
pub fn classify_error_line(
    line: &str,
    sig: &AgentErrorSignatures,
) -> Option<AgentErrorKind>;

/// Per-session error state cleared on next normal output.
#[derive(Debug, Clone, Default)]
pub struct AgentErrorState {
    pub error_active: bool,
    pub last_signature: Option<String>,
}

impl AgentErrorState {
    /// Record a match: set error_active and note the signature.
    pub fn note_error(&mut self, sig: &str);

    /// Clear state because the agent resumed normal output.
    pub fn clear_on_resume(&mut self);
}
```

**Default signatures** (shipped, overridable per config):

```rust
pub const DEFAULT_AGENT_ERROR_SIGNATURES: &[&str] = &[
    "weekly limit",
    "rate limit",
    "usage limit",
    "limit reached",
    "quota exceeded",
    "out of credits",
    "insufficient credits",
    "credit balance",
    "billing",
    "payment required",
    "connection error.",
    "connection refused",
    "connection timed out",
    "network error",
    "network request failed",
    "authentication failed",
    "permission denied",
];
```

Deliberately EXCLUDED: bare `Error:` prefix, `Command failed`, stack
traces, HTTP status codes — these are transient tool-call noise.

### Step 2: `crates/thegn-core/src/lib.rs`

Add `pub mod agent_error;` in alphabetical position (after `activity`
and `activity_step`; before `attention`).

### Step 3: `crates/thegn-core/src/config.rs`

Add to `NotificationsConfig`:

```rust
/// Substrings that classify a live agent output line as a harness
/// failure banner. Each entry is matched case-insensitively against
/// individual output lines. Defaults to thegn's known harness banners;
/// add your harness's own to catch e.g. "your claude subscription…".
#[serde(default = "default_agent_error_signatures")]
pub agent_error_signatures: Vec<String>,
```

The `default_agent_error_signatures` function wraps
`AgentErrorSignatures::defaults().signatures`.

Also add the config validation in `config_validate.rs` or the
`NotificationsConfig::validate` method (if one exists; if not,
add inline validation: reject empty-string entries, reject over-long
entries > 256 chars).

### Step 4: `crates/thegn-core/src/attention.rs`

Add `agent_error_active: bool` to `AttentionInputs`:

```rust
pub struct AttentionInputs {
    // ... existing fields ...
    /// Whether the live agent has emitted a harness failure banner
    /// that has not yet been cleared by resumed normal output.
    #[serde(default)]
    pub agent_error_active: bool,
}
```

In `score()`: after collecting all signal tiers, if
`agent_error_active` is true and the current best tier is below
Failure, bump to `(T::Failure, R::AgentFailed)` with sub-priority
`3.5` (between `ProcessFailed` at sub=2 and `CiFailed` at sub=4):

```rust
if inputs.agent_error_active {
    consider(
        T::Failure,
        3,   // sub: after ProcessFailed, before CiFailed
        R::AgentFailed,
        None, // no since — this is live state, not a stored notification
        0,    // no episode
    );
}
```

## Tests (run these)

```sh
# Scoped crate-level check (no full workspace):
just quick thegn-core

# Specific test suite:
cargo test -p thegn-core agent_error
cargo test -p thegn-core attention::score_with_agent_error
cargo test -p thegn-core -- config::agent_error_signatures
```

### Required test cases

1. `classify_error_line` returns `Some(HarnessBanner)` for
   `"Weekly limit reached (~100% of your plan)"`
2. `classify_error_line` returns `None` for
   `"Error: Command failed with no output"`, `"● Fetch(https://...)"`,
   stack traces
3. Empty signatures → never classifies
4. Case-insensitivity: `"CONNECTION ERROR."` matches
5. `AgentErrorState::note_error` then `clear_on_resume` lifecycle
6. `score()` with `agent_error_active: true` → Failure/AgentFailed
7. `score()` with `agent_error_active: false` → no effect

## Done criteria

- [ ] `cargo test -p thegn-core agent_error` passes (all new tests)
- [ ] `cargo test -p thegn-core -- attention::score` passes (existing + new)
- [ ] `just quick thegn-core` passes (clippy on lib + bin code)
- [ ] Coverage gate: new module is 95%+ line coverage
- [ ] Commit subject: `feat(thegn-core): agent error classification + config key (THE-89)`

## Overlap / dependencies

- **Depends on:** nothing (new module, greenfield)
- **Blocked by:** nothing
- **Blocks:** Chunk 2 MUST land after Chunk 1
- **Parallelism:** none — this is the first chunk
