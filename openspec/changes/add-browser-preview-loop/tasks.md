# Tasks — browser preview loop

## Phase 1 — config + core seam (pure logic, 95% gate)

- [ ] 1.1 `BrowserConfig` (`[browser] pane_command`, `profile_dir_mode`,
      `allow_external_urls`) + `BrowserSnapshotConfig` (`[browser.snapshot]
kind`, `command`, `timeout_ms`, `auto`) in `thegn-core/src/config.rs`
      sibling module (`config/browser.rs` — don't grow the god-file), with
      env overlays and unit tests.
- [ ] 1.2 `thegn_core` snapshot seam: `SnapshotProvider` trait + kind registry
      (`none`/`servo-fetch`/`chromium`/`custom`, implemented-or-`reserved`),
      pure `plan(url, out) -> SnapshotPlan` with template render + shell
      quoting; reject `custom` templates missing `{url}`/`{out}`. Unit tests
      incl. quoting edges.
- [ ] 1.3 Pure URL-confinement check (`navigate` target vs active-forward
      origins / loopback / `allow_external_urls`), unit-tested.
- [ ] 1.4 Profile-dir resolution (`per-workspace`/`per-profile`/`shared` →
      path under XDG state) as pure logic, unit-tested.
- [ ] 1.5 Document every new key in `config/config.toml.example` (including
      the logged-in-profile blast-radius note).

## Phase 2 — host snapshot execution + doctor

- [ ] 2.1 Host-side executor: run the planned argv on `sched::spawn_bg` (QoS
      `Utility`), wrapped via `wrap_background_argv`, with the `timeout_ms`
      watchdog; decode PNG via `rasterize`; deliver `(url, raster)` over a
      channel + waker pulse.
- [ ] 2.2 `thegn doctor` Probe for `[browser.snapshot]` (binary resolution,
      reserved/not-configured verdicts; never fetches).
- [ ] 2.3 Forward-panel snapshot rendering through `preview_gfx` (kitty) with
      text placeholder fallback; completion marks `dirty` (Full), matching
      `preview_pane`.
- [ ] 2.4 Debounced auto-refresh hook off the forward detector diff when
      `[browser.snapshot] auto = true` (no new polling).

## Phase 3 — pane surface + actions + help

- [ ] 3.1 `browser-pane` action: spawn `pane_command` (rendered with `{url}`,
      `{profile_dir}`, env `THEGN_BROWSER_PROFILE_DIR`) as a normal center
      pane through the existing tool-launch path (sandbox/cgroup wrap, daemon
      ownership); hidden from the palette when `pane_command` is empty.
- [ ] 3.2 `browser-snapshot` + `browser-reload` actions (panel-scoped;
      reload re-shoots or signals/respawns the pane).
- [ ] 3.3 `docs/help/browser-preview.md` claiming the three action ids
      (help ratchet + prose ratchet); one-line pointer added to
      `share-and-forward.md`. `panel:forward` context mapping unchanged.
- [ ] 3.4 `THEGN_E2E` freeze: deterministic snapshot placeholder; assert no
      pane tool launches under the freeze.

## Phase 4 — `browser.drive` becomes real

- [ ] 4.1 Daemon handler (`daemon/service.rs`): validate `BrowserCommand`,
      answer `FailedPrecondition("browser preview not configured")` when
      neither surface is configured, else `put_intent("browser_drive", …)` and
      ack (worktrees.open pattern). Unit test the decision fn.
- [ ] 4.2 Compositor intent claim: navigate/reload/back semantics per
      design.md (snapshot retarget/re-shoot; pane respawn fallback; `back`
      no-op-with-status where unsupported), off the model refresh.
- [ ] 4.3 URL-confinement enforcement on `navigate` (Phase 1.3 check) with a
      distinct error before enqueue.
- [ ] 4.4 CLI: `cmd/session.rs` drive verb now surfaces real outcomes; update
      its help text. No new catalog rows; scope stays
      `required_scope(Verb::DriveBrowser)`.
- [ ] 4.5 Reconcile coverage bookkeeping **through**
      `complete-control-surface-coverage`'s artifacts (its `stub` marker /
      `SURFACE_GAPS` ratchet lines for `browser.drive`), whichever lands
      first; MCP tool remains with the write-tools branch, plugin surface via
      THE-39's generic dispatch.

## Phase 5 — integration + gate

- [ ] 5.1 Smoke-test coverage for the subprocess seams (`test/smoke.sh`
      addition guarded on a fake provider binary; core `cov_ignore` notes for
      exec-only paths).
- [ ] 5.2 If `add-drawer-tool-registry` has landed: register the browser pane
      as a `[[drawer.tools]]`-compatible occupant example in its docs;
      otherwise skip (center tab stands alone).
- [ ] 5.3 `openspec validate add-browser-preview-loop --strict`.
- [ ] 5.4 Run `just ci` once, pre-PR (includes openspec-validate; use
      `THEGN_ALLOW_HEAVY=1` for the deliberate gate run).
