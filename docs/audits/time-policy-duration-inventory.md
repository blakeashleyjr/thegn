# Config duration bounds inventory (THE-483/THE-484)

The table records duration-valued numeric configuration fields, not epoch
fields or provider timestamps. Schema maxima document supported strict input;
they do not introduce serde rejection or whole-file runtime fallback.
Cadences use 31 days; other durations use ten 365-day years in the field's own
unit. Existing feature-specific floors remain in their effective-value methods.
Zero keeps each field's existing semantics: polling methods floor it, optional
feature schedules use `None`, calendar account zero inherits the global value,
and TTL/lifetime/window/lease zero retains its documented disable/unlimited
meaning. No shared helper infers a feature's zero meaning.

This is an upper representation boundary, not a claim every bounded timeout is
operationally desirable. Calendar horizons, cleanup retention days, floating
activity grace windows, and millisecond transport/preview timeouts are recorded
separately so they cannot accidentally inherit seconds units or polling floors.
Direct programmatic values still require safe consumer arithmetic.

| Source                                        | Field                          | Type          | Family                 | Maximum constant      |
| --------------------------------------------- | ------------------------------ | ------------- | ---------------------- | --------------------- |
| `crates/thegn-core/src/config.rs`             | `agent_timeout_secs`           | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `merged_ttl_secs`              | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `agent_timeout_secs`           | `Option<u64>` | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `merged_ttl_secs`              | `Option<u64>` | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `max_duration_secs`            | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `keyframe_interval_ms`         | `u64`         | duration milliseconds  | `MAX_DURATION_MILLIS` |
| `crates/thegn-core/src/config.rs`             | `idle_threshold_ms`            | `u64`         | duration milliseconds  | `MAX_DURATION_MILLIS` |
| `crates/thegn-core/src/config.rs`             | `seek_step_secs`               | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `seek_step_video_secs`         | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `poll_interval_secs`           | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `history_days`                 | `u32`         | retention/horizon days | `MAX_DURATION_DAYS`   |
| `crates/thegn-core/src/config.rs`             | `poll_interval_secs`           | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `auto_fetch_interval_secs`     | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `auto_fetch_min_interval_secs` | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `auto_fetch_interval_secs`     | `Option<u64>` | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `auto_fetch_min_interval_secs` | `Option<u64>` | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `refresh_secs`                 | `f64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `sustain_secs`                 | `u32`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `repeat_secs`                  | `u32`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `test_timeout_secs`            | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `discover_timeout_secs`        | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `scan_interval_secs`           | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `idle_clean_days`              | `u32`         | retention/horizon days | `MAX_DURATION_DAYS`   |
| `crates/thegn-core/src/config.rs`             | `restore_grace_secs`           | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `ttl_secs`                     | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `pr_interval_secs`             | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `ready_timeout_secs`           | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `poll_secs`                    | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `pr_ttl_secs`                  | `Option<u64>` | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config.rs`             | `watch_pr_interval_secs`       | `Option<u64>` | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `metrics_timeout_ms`           | `Option<u64>` | duration milliseconds  | `MAX_DURATION_MILLIS` |
| `crates/thegn-core/src/config.rs`             | `disk_scan_interval_secs`      | `Option<u64>` | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `disk_idle_clean_days`         | `Option<u32>` | retention/horizon days | `MAX_DURATION_DAYS`   |
| `crates/thegn-core/src/config.rs`             | `loc_scan_interval_secs`       | `Option<u64>` | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config.rs`             | `loc_watch_invalidate_secs`    | `Option<u64>` | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_activity.rs`    | `runaway_secs`                 | `f64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_activity.rs`    | `quiet_grace_secs`             | `f64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_activity.rs`    | `resume_grace_secs`            | `f64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_activity.rs`    | `spawn_grace_secs`             | `f64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_activity.rs`    | `unsolicited_gap_secs`         | `f64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_activity.rs`    | `output_hint_ttl_secs`         | `f64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_automations.rs` | `action_timeout_secs`          | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_automations.rs` | `action_timeout_secs`          | `Option<u64>` | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_automations.rs` | `debounce_secs`                | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_automations.rs` | `idle_secs`                    | `Option<u64>` | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_autopilot.rs`   | `agent_timeout_secs`           | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_autopilot.rs`   | `agent_timeout_secs`           | `Option<u64>` | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_calendar.rs`    | `ttl_secs`                     | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_calendar.rs`    | `refresh_interval_secs`        | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_calendar.rs`    | `horizon_past_days`            | `u32`         | retention/horizon days | `MAX_DURATION_DAYS`   |
| `crates/thegn-core/src/config_calendar.rs`    | `horizon_future_days`          | `u32`         | retention/horizon days | `MAX_DURATION_DAYS`   |
| `crates/thegn-core/src/config_calendar.rs`    | `refresh_interval_secs`        | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_calendar.rs`    | `timeout_secs`                 | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_ci.rs`          | `ttl_secs`                     | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_ci.rs`          | `poll_interval_secs`           | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_daemon.rs`      | `idle_exit_secs`               | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_daemon.rs`      | `lease_grace_secs`             | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_env_tables.rs`  | `max_lifetime_secs`            | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_env_tables.rs`  | `hibernate_idle_secs`          | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_env_tables.rs`  | `interval_secs`                | `f64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_env_tables.rs`  | `timeout_ms`                   | `u64`         | duration milliseconds  | `MAX_DURATION_MILLIS` |
| `crates/thegn-core/src/config_env_tables.rs`  | `idle_ttl_secs`                | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_env_tables.rs`  | `hibernate_after_secs`         | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_env_tables.rs`  | `max_idle_secs`                | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_issues.rs`      | `ttl_secs`                     | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_loc.rs`         | `scan_interval_secs`           | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_loc.rs`         | `watch_invalidate_secs`        | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_model_proxy.rs` | `window_secs`                  | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_model_proxy.rs` | `first_byte_timeout_secs`      | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_model_proxy.rs` | `idle_timeout_secs`            | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_model_proxy.rs` | `heartbeat_secs`               | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_network.rs`     | `recovery_probe_secs`          | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_observe.rs`     | `refresh_interval_secs`        | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_pipeline.rs`    | `timeout_secs`                 | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_pipeline.rs`    | `backoff_ms`                   | `u64`         | duration milliseconds  | `MAX_DURATION_MILLIS` |
| `crates/thegn-core/src/config_placement.rs`   | `scale_down_idle_secs`         | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_placement.rs`   | `cooldown_secs`                | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_placement.rs`   | `headroom_ttl_secs`            | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_pr_queue.rs`    | `poll_interval_secs`           | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_pr_queue.rs`    | `agent_timeout_secs`           | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_pr_queue.rs`    | `poll_interval_secs`           | `Option<u64>` | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_pr_queue.rs`    | `agent_timeout_secs`           | `Option<u64>` | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_preview.rs`     | `fetch_timeout_ms`             | `u64`         | duration milliseconds  | `MAX_DURATION_MILLIS` |
| `crates/thegn-core/src/config_preview.rs`     | `fetch_timeout_ms`             | `Option<u64>` | duration milliseconds  | `MAX_DURATION_MILLIS` |
| `crates/thegn-core/src/config_remote.rs`      | `keepalive_interval_secs`      | `u32`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_remote.rs`      | `connect_timeout_secs`         | `u32`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_remote.rs`      | `control_persist_secs`         | `u32`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_remote.rs`      | `retry_base_delay_ms`          | `u64`         | duration milliseconds  | `MAX_DURATION_MILLIS` |
| `crates/thegn-core/src/config_remote.rs`      | `retry_max_delay_ms`           | `u64`         | duration milliseconds  | `MAX_DURATION_MILLIS` |
| `crates/thegn-core/src/config_voice.rs`       | `max_seconds`                  | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_vpn.rs`         | `ready_timeout_secs`           | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_weather.rs`     | `refresh_interval_secs`        | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/config_weather.rs`     | `stale_after_secs`             | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_weather.rs`     | `hard_expiry_secs`             | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/config_weather.rs`     | `timeout_secs`                 | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/host_config.rs`        | `probe_ttl_secs`               | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/mcp/config.rs`         | `health_interval_secs`         | `u64`         | cadence seconds        | `MAX_CADENCE_SECS`    |
| `crates/thegn-core/src/mcp/config.rs`         | `cooldown_secs`                | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |
| `crates/thegn-core/src/mcp/config.rs`         | `request_timeout_secs`         | `u64`         | duration seconds       | `MAX_DURATION_SECS`   |

Signed host DB record timestamps/TTL storage and config_write authoring DTOs are not serde Config fields; their runtime/write boundaries are reviewed separately.

Additional optional-float mirrors in ConfigOverlay: `metrics_interval_secs` uses MAX_CADENCE_SECS; `activity_runaway_secs` uses MAX_DURATION_SECS. Final merged-Config admission validates their base fields.
