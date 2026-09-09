# Tasks — add-pipeline-board-access

## 1. The action

- [x] 1.1 `Action::OpenPipelineBoard` + `key()`/`from_key()` round trip
      (`open-pipeline-board`, legacy alias `pipeline-board`).
- [x] 1.2 `ActionSpec` in `keymap_specs.rs` (label "Pipeline board", hint
      `pipeline`, palette, keywords).
- [x] 1.3 Default chord `Alt b` in `default_keymap()`, with the reason on the
      line (the `Ctrl Alt` layer is what fails on a legacy terminal).
- [x] 1.4 `MonitorOverlay::goto_tab` — jump an open monitor to a named tab, or
      report that this machine does not show it.
- [x] 1.5 `run.rs`: one `open_pipeline_board!` body shared by the action arm,
      the sidebar row's `↵`, and its click.
- [x] 1.6 `docs/help/system-monitor.md`: claim the id and describe it in the
      Pipeline section.

## 2. The sidebar row

- [x] 2.1 `PipelineSummary` + `SidebarStatus::pipeline` + `SidebarRow::pipeline`.
- [x] 2.2 `monitor_pipeline::summary` — pure fold, rows not worktrees — wired
      into the existing off-loop roster read in `attention_status.rs`.
- [x] 2.3 `RowKind::PipelineSummary` + `build_rows` emission at the tail, gated
      on live rows.
- [x] 2.4 `sidebar_view`: full-width and rail renderings, tokens + `GlyphSet`
      only.
- [x] 2.5 `sidebar_keys` / `sidebar_mouse`: `↵` and click both synthesize the
      action through `SidebarOutcome::Synthetic`.
- [x] 2.6 Tests: the row appears only with live rows, is not a target/markable/
      collapsible, and sits below every workspace row and above `TERMINALS`.

## 3. The masthead door

- [x] 3.1 A second click on a chip whose popup is open expands into the monitor
      at `MonitorTab::for_widget`'s tab; the first click still opens the popup.
- [x] 3.2 `uptime` maps to the CPU (machine-overview) tab.
- [x] 3.3 Opening a stat popup by click names the destination in the statusbar.

## 4. The defects underneath

- [x] 4.1 `util::now_ms` + `put_agent_dispatch` writes milliseconds; db test
      pins the magnitude, board test pins the rendered age.
- [x] 4.2 `MonitorPrefs::last_tab` assigned on every tab move; tab switches
      report `PrefsChanged` so the loop persists them. Test.
- [x] 4.3 `MonitorOutcome::Passthrough`: Alt/Super (incl. `Ctrl Alt`) and
      `Ctrl-g` hand back; `Ctrl-C` still closes; other plain `Ctrl` still
      consumed. Loop falls through on `Passthrough`. Test.
- [x] 4.4 `open-monitor` palette keywords name the containers and pipeline tabs.
- [x] 4.5 `parse_chord`'s case rule documented and pinned by a test (see
      design.md — the requested lower-casing was refused, with the collision
      list).

## 5. Gates

- [x] 5.1 `cargo check -p thegn-host -p thegn-core --tests`.
- [x] 5.2 Action family, help ratchets, palette, sidebar, monitor,
      monitor_pipeline, keymap and core dispatch tests.
- [x] 5.3 `treefmt`, `openspec validate --strict`.
