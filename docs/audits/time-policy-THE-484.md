# THE-484 signed duration and provider-age boundary review

The original unsigned-to-signed casts could turn a large lifetime, cache TTL,
lease grace, circuit-breaker cooldown or budget window negative. Several
subsequent epoch additions/subtractions also overflowed independently of the
cast. Provider creation time could be missing/malformed or in the future, while
legacy ledger created_at sometimes represented local finalization time.

The fix separates three operations: saturating nonnegative duration conversion
for existing signed APIs; checked provider-relative deadlines (unknown on invalid
or unsupported input); and checked provider age before destructive policy. Cache
freshness tolerates clock rollback without repeated repolling. Feature zero
semantics remain explicit. Remaining cast classifications are below.

## Resource decisions

VPS inventory supplies authoritative creation timestamps. Unknown, nonpositive,
future or unsupported lifetime input becomes an observable quarantine warning
and cannot authorize expiry or orphan deletion. A ledger record with a different
provider/instance ID, or an empty recorded instance ID, also quarantines. Invalid local provisioning/hibernation
ages cannot trigger destructive recovery. An absent live VPS may still retire
an existing ready ledger row because that decision does not depend on age.

Fly legacy created_at is never treated as provider time. Off-thread reconciliation
reads the app's Machines inventory using the existing authenticated HTTP seam.
The exact persisted Machine ID, Machine name and existing ownership metadata
must match, and inventory must contain exactly one well-formed machine. Empty,
multiple, malformed or changed inventories quarantine. Provider created_at uses
the documented API timestamp. The app name is derived separately from sandbox
name and is not compared with Machine name. The existing managed SSH custody
record must exist and match provider, account, instance, key path and key fingerprint before any inventory read. A failed read retains the
ledger for retry; a staged record without exact machine identity quarantines.
No local timestamp provenance is invented. [Fly Machines API response fields](https://fly.io/docs/machines/api/machines-resource/)

Primary review caught an absent-custody `Ok(None)` admission; the new injected
inventory seam rejects absent or mismatched custody without reading the provider.
Independent review caught per-environment lifetime/account ambiguity: the legacy
ledger does not identify its environment. Both reapers now quarantine an entire
provider kind before provider construction, inventory or ledger cleanup whenever
its configured endpoint, credential reference or lifetime differs across envs.
This includes a 60-second policy beside disabled expiry. Exact duplicate policies
still reconcile once. Credential/endpoint spellings are compared conservatively;
aliases that might refer to one account can quarantine rather than merge uncertain
authority. Staged VPS intent without a nonempty observed ID cannot authorize
resource deletion; cleanup after confirmed absence remains a separate decision.

The last inventory read and provider deletion remain separate operations; this
change does not eliminate a resource-identity race between them or repair the
broader provider generation-authority contract. Quarantine may retain a billed
resource until identity/time is reconciled, but it is visible and retryable.
No live provider or process action was executed during this work.

## Consumer coverage

| Family | Result |
| --- | --- |
| Fly/VPS lifetime and orphan/staged-age decisions | Checked age; invalid input quarantines before lifecycle actions |
| Warm-pool stale provisioning / hibernation recovery | Invalid/future local times quarantine; no age-derived destroy |
| CI/calendar/weather/disk scan/LOC/Git/placement TTL | No lossy signed TTL comparison; future cache timestamps remain quiet |
| Daemon lease, MCP breaker, model budget | Wide conversion before saturation; positive extreme duration cannot become disabled/negative |
| Model budget storage | Same checked window lapse policy as reads; no raw subtraction overflow |
| Usage resets/forecast; proxy headers/body resets | Oversized/nonfinite relative delay is unknown; epoch addition is checked |
| Proxy/CI/remote/placement health backoff | Saturating deadline arithmetic; no negative wrap |
| Backoff jitter and Git commit calendar | Wide intermediate arithmetic preserves nanosecond/epoch boundaries |
| Epoch/elapsed measurements in core/host/service/proxy | Saturate after widening; absolute epochs are not capped as durations |
| Media MPRIS duration | Out-of-range unsigned input is rejected instead of wrapping negative |
| Pairing-code relative lifetime | Checked before mint/persistence; unsupported duration cannot produce a past deadline |

Standalone actual-source tests currently pass: shared time/schedule/panic 11,
backoff 13 and git calendar 5. Full consumer/core/service/proxy/media tests,
combined host gates, final primary and independent reviews remain pending.

## Remaining `as i64` classification

Repository-wide source scan includes tests and non-time fields. This table classifies every remaining file with such a cast; it does not assert unrelated token/count/byte narrowing is safe or fixed. Line numbers are review-checkpoint locations.

| File | Lines | Classification |
| --- | --- | --- |
| `crates/gtui-embed/src/embed.rs` | 87 | Float-derived interval uses saturating float conversion followed by the existing60..86400-second clamp. |
| `crates/thegn-core/src/backoff.rs` | 241, 366 | Wall-clock nanoseconds are reduced modulo2^63 only as entropy, never used as an epoch/duration; other cast is a bounded jitter test. |
| `crates/thegn-core/src/calendar/cursor.rs` | 134 | Calendar year i32/month u32/delta i32 widen losslessly before month arithmetic. |
| `crates/thegn-core/src/calendar/grid.rs` | 58, 59 | Weekday values0..6 widen losslessly. |
| `crates/thegn-core/src/calendar/ics.rs` | 445 | Parsed trigger minutes are u32 and widen losslessly; minute-to-second magnitude fits chrono::Duration. |
| `crates/thegn-core/src/calendar/recur.rs` | 491, 492, 493, 686, 721, 723, 726, 826, 828, 831 | Recurrence interval is u32, ordinal/day fields are narrow signed/calendar values; widening and chrono unit multiplication fit. No u64 duration narrowing remains. |
| `crates/thegn-core/src/config_duration.rs` | 575 | Test-only fixtures/expected values; no production duration boundary. |
| `crates/thegn-core/src/db_account.rs` | 58 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_automation.rs` | 223, 238 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_autopilot.rs` | 56, 65, 101, 123, 155, 168 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_aux.rs` | 233, 259, 281, 282, 306, 556, 802 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_cache.rs` | 380, 407 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_calendar.rs` | 120, 180 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_compute.rs` | 221 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_dispatch.rs` | 467 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_migrate.rs` | 140, 156 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_model_proxy.rs` | 220, 231 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_notification.rs` | 216 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_placement.rs` | 203, 204, 205, 206, 277, 278, 279, 280, 282, 283, 321, 322, 323, 324, 436, 529, 539 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_projects.rs` | 148 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/db_workspace.rs` | 271, 654, 703, 1373, 1393, 1441 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/graveyard.rs` | 181 | Remaining casts occur in fixed/default-bound test fixtures; production duration conversions were replaced. |
| `crates/thegn-core/src/host_db.rs` | 389, 441 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/merge_sweep.rs` | 109, 116, 145, 146, 147, 161, 162 | Remaining casts occur in fixed/default-bound test fixtures; production duration conversions were replaced. |
| `crates/thegn-core/src/migrate_brand.rs` | 167, 168 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/patch.rs` | 578 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/proxy/creds.rs` | 108 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/proxy/stats.rs` | 138 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/proxy/transform.rs` | 109 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/proxy/translate.rs` | 804 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-core/src/sandbox_cpucap.rs` | 264 | Non-time display/control quantity (weather, volume or CPU percentage), using float-to-integer saturation. |
| `crates/thegn-core/src/scan_sched.rs` | 186, 187, 215, 217 | Remaining casts occur in fixed/default-bound test fixtures; production duration conversions were replaced. |
| `crates/thegn-core/src/spillover.rs` | 87 | Retry-After is capped at3600 seconds before multiplying by1000 (maximum3600000ms). |
| `crates/thegn-core/src/time_policy.rs` | 155, 307 | Runtime narrowing is explicitly capped at i64::MAX; remaining uses are fixed-bound assertions. |
| `crates/thegn-core/src/usage.rs` | 396 | Comment describing the removed truncating cast; no remaining cast expression. |
| `crates/thegn-core/src/weather.rs` | 387, 396 | Non-time display/control quantity (weather, volume or CPU percentage), using float-to-integer saturation. |
| `crates/thegn-core/tests/sandbox_audit.rs` | 17 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/attention_status.rs` | 140, 290, 294, 298 | Activity timestamps are floating-point display/scoring projections (Rust saturating float cast, no modular unsigned wrap); other cast is a row ordinal. |
| `crates/thegn-host/src/autoscale.rs` | 448 | Remaining casts occur in fixed/default-bound test fixtures; production duration conversions were replaced. |
| `crates/thegn-host/src/center.rs` | 629, 630, 635, 636 | Screen coordinates, row/tab ordinals or selection indices; no runtime duration narrowing. |
| `crates/thegn-host/src/chrome_tests.rs` | 3017, 3027 | Test-only fixtures/expected values; no production duration boundary. |
| `crates/thegn-host/src/cmd/disk.rs` | 49 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/cmd/list.rs` | 83 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/daemon/mod.rs` | 291 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/daemon/session.rs` | 825 | Remaining state-since projection is a floating epoch for an activity event; Rust saturates float-to-int conversion. Destructive clocks use checked policy separately. |
| `crates/thegn-host/src/detail.rs` | 1009 | Screen coordinates, row/tab ordinals or selection indices; no runtime duration narrowing. |
| `crates/thegn-host/src/detail/calendar/layout.rs` | 141 | Screen coordinates, row/tab ordinals or selection indices; no runtime duration narrowing. |
| `crates/thegn-host/src/detail/calendar/mod.rs` | 286, 290, 321, 322, 326 | Screen coordinates, row/tab ordinals or selection indices; no runtime duration narrowing. |
| `crates/thegn-host/src/detail/calendar/render.rs` | 455, 458, 543 | Screen coordinates, row/tab ordinals or selection indices; no runtime duration narrowing. |
| `crates/thegn-host/src/detail_tests.rs` | 122, 1846, 1909 | Test-only fixtures/expected values; no production duration boundary. |
| `crates/thegn-host/src/handlers/provision.rs` | 764 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/handlers/sidebar_reorder.rs` | 343, 1414 | Screen coordinates, row/tab ordinals or selection indices; no runtime duration narrowing. |
| `crates/thegn-host/src/hydrate_weather_tests.rs` | 71 | Test-only fixtures/expected values; no production duration boundary. |
| `crates/thegn-host/src/i18n_surface.rs` | 113, 118 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/measure/disk.rs` | 109 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/monitor_tests.rs` | 87, 918 | Test-only fixtures/expected values; no production duration boundary. |
| `crates/thegn-host/src/pane.rs` | 2576, 2585, 2588 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/panel/sections/merge_queue.rs` | 319 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/platform/windows.rs` | 373 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/pr_driver.rs` | 1171 | Remaining casts occur in fixed/default-bound test fixtures; production duration conversions were replaced. |
| `crates/thegn-host/src/sections.rs` | 231, 277, 298, 327, 366, 416, 531, 650, 653 | Screen coordinates, row/tab ordinals or selection indices; no runtime duration narrowing. |
| `crates/thegn-host/src/session.rs` | 196, 546, 547, 550 | Screen coordinates, row/tab ordinals or selection indices; no runtime duration narrowing. |
| `crates/thegn-host/src/sidebar.rs` | 4195 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
| `crates/thegn-host/src/sidebar_view.rs` | 186, 188 | Screen coordinates, row/tab ordinals or selection indices; no runtime duration narrowing. |
| `crates/thegn-host/src/vps_reaper.rs` | 308 | Remaining casts occur in fixed/default-bound test fixtures; production duration conversions were replaced. |
| `crates/thegn-media/src/applescript.rs` | 122 | Non-time display/control quantity (weather, volume or CPU percentage), using float-to-integer saturation. |
| `crates/thegn-media/src/platform/linux/mpris.rs` | 299, 334, 638, 639, 640, 641 | Microsecond duration output is explicitly capped; integer variants are lossless i16/u16/i32/u32 widening. u64 input now uses try_from. |
| `crates/thegn-media/src/smtc.rs` | 339 | 100ns duration units are explicitly capped at i64::MAX before narrowing. |
| `crates/thegn-proxy/src/reset.rs` | 109 | Finite nonnegative total milliseconds are checked against the10-year limit before ceil/narrowing. |
| `crates/thegn-proxy/src/router.rs` | 722, 805, 806, 807, 808, 854 | Non-time IDs/counts/bytes/capacities/booleans/token fields, plus any test fixtures. Outside duration/epoch policy scope. |
