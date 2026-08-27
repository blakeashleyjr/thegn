# Fix: an OSC raised hand is live state, and "clear all" clears what it shows

Linear: THE-68

## Why

Two symptoms, reported together, sharing one pile of rows.

**1. The inbox fills with "Claude is waiting for your input".** Every PTY byte
funnels through `SessionActor::on_output`, which feeds
`thegn_core::osc_attention::OscAttentionScanner`; a hit calls `on_attention`
(`crates/thegn-host/src/daemon/session.rs:741`), which sets the live
`self.attention`, publishes the feed edge — and then appends an
`agent_attention` **notification row**. Claude Code and friends emit
`OSC 9` / `OSC 777;notify` at the **end of every turn**, so the append-only
`notifications` table gains one row per turn, per agent, forever. With a handful
of agents that is hundreds of rows a day, and they are the only thing in the
inbox.

The row exists because it is being used as a **cross-process channel for live
state**: an unread `agent_attention` row is what raises the attention tier
(`attention_status.rs:149` → `attention.rs:484` → `(Blocked, AgentNeedsInput)`).
Using an append-only log that way has three consequences, all of them defects:

- **unbounded rows** — live state has one value, a log has one row per edge;
- **it never clears** — `on_input` clears the in-memory `self.attention`, but
  the DB row stays unread, so the worktree scores `Blocked` until the user marks
  it read by hand. `openspec/changes/add-osc-attention-signaling/`'s
  attention-signals delta specifies the opposite ("**Scenario: Resume clears the
  signal**"); the persisted half violates the contract the live half satisfies;
- **it bypasses routing** — `notify::record` (`crates/thegn-host/src/notify.rs:286`)
  is the one funnel where `[[notifications.rules]]`, DND, debounce, toast, sound
  and push are decided. The daemon calls `put_notification` directly, so the
  noise cannot be muted with a rule and the burst suppression that already
  exists for `process_failed` never applies.

**2. "Clear all" leaves rows behind.** Two functions decide what belongs to the
active repo's inbox and they disagree. Display
(`hydrate_feed::populate_notifications`) is deliberately **fail-open** — a row
tagged with a worktree path the registry does not know (the repo's own **main
checkout**, which never gets a `worktrees` row; an externally-created worktree;
a renamed path) is kept, not hidden. Clear
(`handlers::attention::mark_all_read` → `Db::mark_notifications_read_scoped`)
is **fail-closed** — untagged rows plus this repo's registered paths, nothing
else. So the fail-open set is displayed and never cleared: `a` optimistically
greys those rows, the next hydration re-reads the DB, and they come back unread.

The two symptoms meet in the same rows. The OSC producer writes its session's
worktree path verbatim, and the main checkout has no registry row — so the
un-clearable pile _is_ the "Claude is waiting" pile. They are still independent
bugs: fixing either alone leaves the other.

## What Changes

The thesis, and the line both fixes are drawn from:

> An **inbox row is an event you might otherwise miss**. A **raised hand is live
> state you can already see.** Ambient OSC chatter is the second kind; a
> deliberate push (`thegn notify push`, `notify.push` over the control API, MCP
> `request_human`) is the first.

- **An OSC attention signal becomes live session state.** A new
  `session_attention` table (schema v57) holds one row per daemon session,
  upserted when a hand goes up and deleted the moment the user answers, the
  session ends, the daemon boots with an empty session registry, the worktree is
  removed, or the worktree's needs-you signal is acknowledged/cleared.
- **It scores through the existing reason.** `AttentionInputs` gains
  `attention_signal_since`, and `score` gains one `consider` arm mapping it to
  the **existing** `(Blocked, AgentNeedsInput)` — the same move
  `stage_blocked_since` already makes for pipeline stages parked on a human.
  **No new tier, no new reason, no new notification kind, no new surface.** The
  sidebar dot, the `✋` chip, the "Needs you" popup and the `Alt a` ring behave
  exactly as today; what changes is that answering the agent now clears the
  demand, which is what the contract always said.
- **The `agent_attention` kind is untouched for deliberate pushes.**
  `daemon/service.rs:1112` still maps `urgency = alert` onto it and
  `attention.rs:484` keeps its arm; a real push still lands a real row.
- **`[notifications] agent_attention_inbox` (default `false`)** restores the
  inbox row for anyone who wants the audit trail — same shape as the
  `surface_self_log_errors` flag in the same section. When on, the write is a
  delete-then-insert per session: one **current** row per session, never one per
  turn.
- **A one-time migration** (`ver < 57`, following the v46 `process_failed`
  precedent) marks the accrued unread `agent_attention` backlog read, so the
  inbox and the `⚑`/`✋` counts start clean instead of carrying months of rows.
- **"Clear all" clears exactly what the inbox displays.** The display predicate
  is extracted into one pure, tested `thegn_core::notification_scope` function
  and both call sites project it; `mark_notifications_read_scoped` takes the
  registry set too and its SQL grows the fail-open arm. One predicate, two call
  sites — the asymmetry cannot come back because there is no second copy to
  drift. The clear also lowers the live raised hands for the same scope, or the
  new state would become a new un-clearable nag.

Deliberately **not** in scope: `put_notification("disk_cleaned", …)`
(`hydrate.rs:3596`) uses a kind string absent from `NotificationKind`, so
`notifications_query` falls back to `StatusChanged` and a reclaimed-disk notice
renders as "status changed ⟳". It is adjacent mislabelled inbox noise, but
fixing it means a 27th kind — the pinned `ALL` count, the `default_priority`
totality test, the `hued_glyph` table and the `config.toml.example` prose test.
Different blast radius; recommend a follow-up issue.

## Impact

- **tasks.md:** group **AI (420, 424, 426, 428)** — the notification bus, its
  rules/DND routing and the notification history/center whose inbox this is —
  and group **S (256)**, needs-attention surfacing, whose authoritative OSC/CLI
  signal path this repairs. Also touches the tier model **AQ (524)** reuses.
- **Capabilities:** `activity-signals` — ADDED (an OSC signal is live state, not
  an inbox event). `notifications` — ADDED (clear-all covers exactly the
  displayed set). No other capability touched; no capability-catalog entry, no
  new CLI verb, no new MCP tool.
- **Code, in four chunks** (land order 1 → 2 → 3 → 4; 1, 2 and 4 are mutually
  independent):
  1. **"Clear all" clears exactly what the inbox shows** — new pure
     `thegn-core/src/notification_scope.rs`, the `mark_notifications_read_scoped`
     signature + SQL, and both call sites (`hydrate_feed.rs`,
     `handlers/attention.rs`).
  2. **Core: the `session_attention` table + the config knob** — `osc_attention.rs`
     row type, `db.rs` DDL/version/migration/prune, six `NotificationStore`
     methods, `del_worktree` cascade, `NotificationsConfig.agent_attention_inbox`
     with its `THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX` override and
     `config.toml.example` prose. Wires nothing; behaviour unchanged after it.
  3. **Wire the live signal** — `attention.rs` input + `consider` arm,
     `daemon/session.rs` producer (upsert / clear on input / clear on session
     end / clear-all on registry boot), `attention_status.rs` consumer,
     `actions.rs` per-worktree ack, `handlers/attention.rs` clear-all.
  4. **Specs, help prose, changelog** — this change folder, `docs/help/bars.md`,
     `docs/help/panel.md`, `CHANGELOG.md`.
- **Gates:** `SCHEMA_VERSION` 56 → 57 (a known cross-branch collision point —
  rebase immediately before landing); one new `[notifications]` shallow key,
  which needs `config/config.toml.example` (`tests/config_example.rs`) **and** a
  real `env_overlay` knob, because `test/env-overlay-ratchet.txt` is shrink-only;
  `db*.rs` is not in the justfile `cov_ignore` list, so the new store methods
  need `db_tests.rs` coverage against the 95% core gate. No new action id,
  chord, zone or panel section ⇒ no `ACTION_SPECS` change and no help-ratchet
  churn.
- **In-flight reconciliation:** `add-osc-attention-signaling` is **still
  unarchived** while most of it has landed — this change does **not** archive it
  and does **not** edit its deltas. It repairs the half of that change's own
  "Resume clears the signal" scenario that the persisted path never satisfied.
  There is no live `openspec/specs/attention-signals/` capability (the OSC
  behaviour was never synced), so the requirement is added to the nearest live
  capability, `activity-signals`.
