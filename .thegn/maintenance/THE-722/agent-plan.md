# THE-722 + THE-405 (engine slice): Observe idle while hidden

Scope: a hidden Observe tab causes no wakes, queries, samples or spawns; wakes coalesce to one pending per tile.

- tg-kit `AppTile::on_visible(bool)` (default no-op); host `AppHost::sync_visibility` (per-slot told-flag, change-only) called each loop iteration.
- gtui-app engine: `EngineCmd::Visible`, `WakeGate` (visible flag silences mid-cycle wakes; pending flag coalesces wakes until the UI drains). Ticker branch disabled while hidden; show resets ticker and refreshes immediately.
- gtui-core `DataSource::set_active` (default no-op); `HostSource` parks its sampler thread untimed while hidden.
- Tests: hidden engine yields zero wakes/queries, resumes immediately; wake coalescing; sampler park/resume/drop; sync_visibility change-only.

Out of scope (rest of THE-405): bounded channels, per-slot dirty bitset/generation in the host hook, drain budget, standalone harness.
