# Tasks

## 1. Local usage/rate-limit reader (superzej-core, item 759)

- [ ] 1.1 Extend `account.rs::Provider` with a `usage_file` + a pure `parse_usage`
      seam returning `UsageState { plan_pct, reset_at, window }` from the resolved
      credential-home dir — **unit tests** against `~/.claude` / `~/.codex`
      fixture files (present, malformed, absent ⇒ `None`).
- [ ] 1.2 `UsageState::warns()` (≥80% of plan) + a short chip label — **unit tests**
      for the 80% boundary and the no-data case.
- [ ] 1.3 Resolve usage for the active worktree via the existing
      worktree/workspace/global account layering — **unit tests** that the
      resolved provider/dir matches the active account.

## 2. Off-loop usage refresh + widget (host, items 759 / L 148–150)

- [ ] 2.1 Add `RefreshKind::Usage` + `spawn_usage_cache_refresh`; emit on the
      ~30–60s ticker cadence (whole multiple of the 500ms half-tick) and pulse the
      `TerminalWaker` — **unit test** the cadence-multiple invariant.
- [ ] 2.2 Statusbar usage chip in the chrome model; chrome-dirty ⇒ Full frame,
      no-delta ⇒ Skip — **render test** that an unchanged usage state yields Skip
      and a changed one yields Full.

## 3. OSC-title agent-state detection (items 760 / 257)

- [ ] 3.1 Pure `classify_title(title) -> Option<ActivityState>` mapping OSC
      0/2 title text to working/idle/waiting — **unit tests** over representative
      titles and the no-match (`None`) case.
- [ ] 3.2 Wire `activity::poll_and_save_with` to prefer the title signal and fall
      back to the CPU heuristic only when no title is present — **unit tests** that
      title overrides CPU and that absent-title preserves the heuristic + sticky-red.

## 4. Session history + one-click resume (item 761)

- [ ] 4.1 `SessionHistoryBackend` trait + per-provider scanners over native
      transcript dirs yielding `{ cwd, branch, model, tokens, first_ask, mtime }`
      — **unit tests** against fixture transcript dirs (parse + sort + cap).
- [ ] 4.2 `agent_sessions` cache table + `user_version` bump; populate off-loop,
      evict rows whose transcript file vanished — **unit tests** for the additive
      migration and the missing-file eviction.
- [ ] 4.3 Resume action shells the provider's own command with `--resume` as a
      normal pane launch — **unit test** the constructed argv per provider.

## 5. Agents feed (item 762)

- [ ] 5.1 New agent state-change `Event` variant; pure feed assembly grouping
      `EventBus` events into per-worktree threads, newest-first — **unit tests**
      for grouping, ordering, and a running-agent smart-pin predicate.
- [ ] 5.2 `Section::Agents` feed + click-to-jump-to-pane mapping; the feed badge
      is chrome-dirty ⇒ Full, no-delta ⇒ Skip — **render test** for the
      Skip/Full decision.

## Validate

- [ ] Run `just ci`
