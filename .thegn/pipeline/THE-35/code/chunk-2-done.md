# THE-35 chunk 2 completion

Implemented the host-side configurable sound path:

- Replaced the synthesized `chime` implementation with a platform-owned,
  synchronous `SoundPlayer` seam for fixed-argv `afplay`, `paplay`, `aplay`,
  PipeWire, ffplay, SoX, and PowerShell providers.
- Added immutable pack/file snapshots, provider capability reporting, bounded
  `try_send` playback jobs, Utility-QoS `notify-sound` workers, drop counters,
  and terminal-BEL fallback for degraded playback.
- Removed the startup event-bus sound subscriber and migrated host producers to
  the single notification route; DB-backed routing records before transient
  sound/toast/push emission.
- Added live `session_attention` baseline/edge/clear handling without creating
  duplicate inbox rows.
- Added sound provider/pack/capability/fallback data to doctor text and JSON,
  and removed `chime.rs` from the platform-cfg ratchet.

Verification:

- `just quick thegn-host` — passed (using isolated `/tmp` runtime settings and
  without the restricted sccache wrapper).
- `cargo nextest run -p thegn-host notification_sound` — 2 passed.
- `cargo nextest run -p thegn-host notify` — 13 passed.
- `cargo nextest run -p thegn-host attention_status` — 9 passed.
- `cargo nextest run -p thegn-host doctor` — 19 passed.
- `cargo nextest run -p thegn-host platform_cfg` — 1 passed.

## Unverified

- No full-workspace `just test`, `just ci`, coverage, or e2e run was performed,
  as prohibited by the chunk and dev-loop policy.
- Cross-platform provider execution was not run on macOS or Windows; fixed argv
  construction is covered by host unit tests and platform conditionals pass the
  host platform ratchet.
