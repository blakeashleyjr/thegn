# Tasks — calendar and world clocks

## 1. Clock accuracy and the render-path panic (independent)

- [x] 1.1 `config::validate_strftime` + `strftime_needs_seconds`, walking parsed
      `StrftimeItems` rather than substring-matching (so `%T`/`%r`/`%X`/`%s`
      count and `%%S` does not).
- [x] 1.2 Validate both `[bars]` formats in `post_process`; warn and fall back
      per field. `chrono::format()` is lazy and panics at `Display`, i.e. inside
      `masthead_widget` on the render path.
- [x] 1.3 `RefreshKind::ClockTick` on a slot in the _existing_ ticker, firing on
      display-boundary crossings; period derived from the formats.
- [x] 1.4 Loop arm sets `bars_dirty` only; comment the stats compare so the
      clock is not re-coupled to `uptime_secs`.
- [x] 1.5 Render-plan tests: bars-only ⇒ `Incremental`; bars + open detail ⇒
      `Full`; no damage ⇒ still `Skip`.

## 2. Pure core domain

- [x] 2.1 `chrono-tz` pinned at the lock's 0.9 in `[workspace.dependencies]`;
      `iana-time-zone` for the system zone; `chrono`'s `serde` feature.
- [x] 2.2 `calendar/grid.rs` — month matrix, fixed-six-weeks default, ISO week
      numbers via `iso_week()`.
- [x] 2.3 `calendar/cursor.rs` — navigation with the sticky day-of-month.
- [x] 2.4 `calendar/tz.rs` — zone resolution, clock readings, DST-edge policy,
      did-you-mean by edit distance.
- [x] 2.5 `calendar/locale.rs` — `auto` week-start / time-format resolution.
- [x] 2.6 `CalEvent`/`EventTime`/`SourceId`, `dates_in`, `expand_by_date`.
- [x] 2.7 Tests to the coverage gate, including every RFC/DST trap.

## 3. Config

- [x] 3.1 `config_calendar.rs` — `[calendar]`, `[[calendar.clocks]]`,
      `[[calendar.accounts]]`, following `config_issues.rs`.
- [x] 3.2 `config_enum!`s: `CalendarProviderKind`, `WeekStart`, `TimeFormat`;
      bump the pinned marked-definition count deliberately.
- [x] 3.3 Refresh floor in `CalendarAccount::refresh_secs`, not at the ticker.
- [x] 3.4 `validate_calendar` wired into `config_validate`, with suggestions.
- [x] 3.5 Every key documented in `config/config.toml.example`.

## 4. The popup

- [x] 4.1 `Section::MonthGrid` + `DetailContent::Calendar`.
- [x] 4.2 `detail/calendar/layout.rs` — pure geometry, one source for draw and
      hit-test; four density steps then a key-value fallback.
- [x] 4.3 `detail/calendar/render.rs` — grid, agenda, world clocks.
- [x] 4.4 `detail/calendar/keys.rs` — the full navigation surface, in place.
- [x] 4.5 `CalendarDocs` on `PanelDocs`, reached via `StatusCtx.cal`.
- [x] 4.6 Mouse: day cells, chevrons, today chip, wheel — preserving the
      existing `dismiss_overlay_on_click_outside = false` behaviour.
- [x] 4.7 `retick_open` refreshes clocks _and_ `today` (midnight rollover).

## 5. Action, keymap, help

- [x] 5.1 `Action::OpenCalendar` + `ActionSpec` + `Alt d` (a real variant: the
      string-dispatch route fails `action_registry_ids_resolve_to_actions`).
- [x] 5.2 `run.rs` toggle arm, anchoring on whichever of date/clock survived
      width shedding.
- [x] 5.3 `docs/help/calendar.md` + `pages.rs::SOURCES`; cross-link from
      `bars.md`. All three help ratchets green.

## 6. Providers

- [x] 6.1 `CalendarBackend` + `CalendarCaps` + `CalendarError::is_transient` +
      static-dispatch `Inner` + `CalendarRouter` with per-account results.
- [x] 6.2 `EditScope` and per-account `sync_token` present from day one —
      retrofitting either breaks the plugin wire format.
- [x] 6.3 `IcsBackend` — file _or_ vdir directory.
- [x] 6.4 `IcsUrlBackend` — ETag-conditional; a 304 is `unchanged`, not empty.
- [x] 6.5 `CalDavBackend` — `calendar-query` / `sync-collection`, tombstones,
      prefix-insensitive XML, RFC 6578 fallback on a rejected token.
- [x] 6.6 `calendar/ics.rs` in core — unfolding, quoted-param colons, VALARM
      nesting, lenient recovery.
- [x] 6.7 `calendar/recur.rs` — full `BY*` model, RRULE-string serde.
- [x] 6.8 Three expansion bugs the edge-case tests caught, each of which
      silently produced _wrong_ occurrences rather than failing:
      `BYDAY`∩`BYMONTHDAY` also seeded from `BYMONTHDAY` (so `BYDAY=FR;
BYMONTHDAY=13` matched the 13th on any weekday); an out-of-range
      `BYYEARDAY`/`BYWEEKNO` fell through to DTSTART's date instead of
      producing nothing; and sub-daily frequencies read `INTERVAL` as days, so
      `HOURLY;INTERVAL=24` meant every 24 _days_. Sub-daily now steps the clock
      with `BY*` acting as filters, bounded against a runaway `SECONDLY`.

## 7. Persistence

- [x] 7.1 `calendar_events` + `calendar_sync`; `SCHEMA_VERSION` 51 → 52.
- [x] 7.2 `store/calendar.rs` trait + `db_calendar.rs` impl; batch writes in one
      transaction; full replace atomic.
- [x] 7.3 Range query returns every recurrence master regardless of its span.
- [x] 7.4 Prune on the startup sweep, sparing masters.
- [x] 7.5 Migration test: a v51 DB gains the tables and keeps its rows.

## 8. Sync and reminders

- [x] 8.1 `RefreshKind::Calendar` / `CalendarReminders`; ticker slots gated on
      "any account enabled" and on `reminders_enabled`, both floored.
- [x] 8.2 `hydrate_calendar::sync_accounts` — per-account offline gate, TTL
      freshness skip, incremental vs full.
- [x] 8.3 The don't-clobber rules: empty-full-fetch guard, failure leaves cache
      and cursor intact, 304 advances only the stamp.
- [x] 8.4 `calendar/reminders.rs` — half-open due window, clock-jump clamp,
      cancelled events skipped.
- [x] 8.5 `NotificationKind::{CalendarReminder,CalendarChanged}`; delivery
      through the normal notification path so `[[notifications.rules]]` applies
      with no new config.
- [x] 8.6 `has_notification` for restart idempotency via `source_ref`.
- [x] 8.7 The due-check runs on the background lane, not inline. It reads the
      DB, and blocking I/O on the event loop is the one thing the event model
      forbids outright — an earlier draft had it inline with a comment wrongly
      claiming it was cache-only. The window stamp advances on the loop so the
      next window starts where this one ended even if the task is delayed.

## 9. The plugin surface

- [x] 9.1 `plugin/proc.rs` — NDJSON, caps that truncate rather than SIGPIPE,
      process-group kill, stderr captured, git env scrubbed.
- [x] 9.2 `CommandBackend` — env-in/JSON-out, `manifest`/`events`/`log` verbs.
- [x] 9.3 Wake `plugin_api`: `HostContract::negotiate` verbatim,
      `ExtensionPoint::DataSource`, capability grants, denials logged.
- [x] 9.4 `Display` for `ApiVersion`/`Capability`; `params` defaulted so a bare
      `{"method":…}` is valid.
- [x] 9.5 Control plane: `/v1/calendar/{events,clocks}` (read) and source
      ingest (write); `Verb` scope table + its pinned exhaustiveness test.
- [x] 9.6 Plugin-authoring documentation in `docs/help/calendar.md`.

## 10. Validation

- [x] 10.1 `just test` (nextest) green: **4682 passed, 0 failed**. Use nextest,
      not plain `cargo test` — the latter shares a process across tests and a
      different unrelated DB/socket test flakes each run (`ipc::unix_bind`,
      `hydrate::load_or_seed_session`, `agent::tool_drawer_launch`, …), every
      one of which passes in isolation.
- [x] 10.2 `cargo clippy` clean on the touched crates.
- [x] 10.3 `just coverage` — the 95% core gate passes (must run under
      `nix develop`; the devenv shell has no `llvm-tools-preview`).
- [x] 10.4 `just check-cross` — green for `x86_64-pc-windows-gnu` and
      `aarch64-apple-darwin`, confirming `chrono-tz` is pure Rust and the new
      `libc` dependency is correctly unix-gated.
- [x] 10.5 `just ci` — every gate green except `e2e`'s `sidebar` spec:
      lint, deps-audit, build, check-cross, test, doc-check, openspec-validate,
      coverage, smoke, both sandbox-e2e suites, and nix-build.
      `nix-build` needs the new files **`git add`ed** — a flake builds only
      git-tracked content, so untracked modules fail the sandboxed build with
      `E0583` (CLAUDE.md notes this for `nix flake check`; it applies here too).
      The `sidebar` spec is **pre-existing and nondeterministic on this
      machine**: three consecutive runs gave 1/2, 0/2, 0/2 with the assertion
      varying between `toBeVisible` and `toNotBeVisible`, and the unmodified
      base revision — built in a scratch worktree — fails it identically. The
      spec's own comments record earlier runner-specific flakiness of the same
      assertion. Not caused here; out of scope for this change.
- [x] 10.6 Drive the real popup. Done as **rendered tests** rather than a muse
      spec (`detail_tests.rs`): open the popup from both the `date` and `clock`
      bar items, assert the title, the weekday header, the month/year, the world
      clock block and the home row; page a month and assert the frame actually
      changes and that `t` restores it byte-for-byte; assert the sub-grid
      fallback at 28 columns; and assert the whole frame is pure ASCII under a
      forced ASCII terminal.

      A muse spec was written and then **withdrawn**: on this machine keys do
      not reach the compositor in the e2e harness at all — a palette probe typed
      `calendar` and no row appeared, though every `palette: true` action is
      listed there — which is the same reason the pre-existing `sidebar` spec
      fails here. Committing a spec that cannot pass locally is worse than none,
      and the rendered tests cover the same ground deterministically and at
      finer granularity.

      Those tests found two real bugs: the world-clock block silently vanished
      unless `CalendarDocs` came from `from_config` (the home row is now
      synthesized in the popup, so the guarantee holds however the docs were
      built), and the today chip, agenda heading, loading label and reminder
      message all hard-coded Unicode separators instead of going through
      `caps::active_glyphs()`.
