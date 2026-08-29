# Chunk 2 completion

Implemented daemon integration and attention plumbing for THE-89.

- `SessionActor` classifies completed history lines using the configured
  `agent_error_signatures`, raises on harness banners, and clears on the next
  non-matching output chunk.
- Activity events carry `error_active` and the owning worktree. The host starts
  a daemon event-feed bridge that mirrors session error state into the
  hydration-side cache.
- `collect_attention` feeds the cached daemon state into `AttentionInputs`,
  producing the existing `Failure` / `AgentFailed` attention result.
- Added the requested notification configuration example and help page, and
  registered the page in the embedded help catalogue.

## Validation

- `RUSTC_WRAPPER= cargo test -p thegn-host -- error_state_lifecycle` — passed.
- `RUSTC_WRAPPER= cargo test -p thegn-host -- agent_error_active` — passed.
- `RUSTC_WRAPPER= cargo test -p thegn-host -- daemon::session` — 16 passed.
- `RUSTC_WRAPPER= cargo test -p thegn-host -- attention_status` — 8 passed.
- `RUSTC_WRAPPER= cargo test -p thegn-host -- help::pages` — 5 passed.
- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= just quick thegn-host` — passed.

## Unverified

- Manual live-agent verification of the rendered glyph lighting and clearing
  was not run.
- Full workspace gates and e2e were not run, per the scoped dev-loop policy.
- `just help-ratchet-update` was not needed: no notification kind or action was
  added; embedded help consistency tests passed.
