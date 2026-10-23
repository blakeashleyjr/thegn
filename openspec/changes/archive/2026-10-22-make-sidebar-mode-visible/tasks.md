# Tasks

- [x] 1. Add flat/grouped mode to the loop-owned render model.
     Evidence: `crates/thegn-host/src/chrome.rs` (`FrameModel.sidebar_flat`);
     `crates/thegn-host/src/run.rs` (projection from `self.view.flat`).
- [x] 2. Render the current layout in a persistent full-sidebar header chip and
     degrade whole at constrained widths.
     Evidence: `crates/thegn-host/src/sidebar_view.rs`
     (`header_chips`, `draw_sidebar`); tests
     `layout_chip_names_the_layout_left_of_the_sort_chip`,
     `grouped_layout_chip_gives_way_before_the_sort_chip`, and
     `flat_layout_chip_outlasts_the_sort_chip`.
     As built: the label is lowercase `flat`, and `grouped` is shown too.
- [x] 3. Reuse the existing `g` toggle/persistence path and share chip paint
     geometry with its mouse hit target.
     Evidence: `crates/thegn-host/src/handlers/sidebar_keys.rs`
     (`toggle_flat`); `crates/thegn-host/src/sidebar_view.rs`
     (`header_chips`); `crates/thegn-host/src/handlers/sidebar_mouse.rs`
     (`on_left_press`); test
     `clicking_the_header_layout_chip_toggles_flat`.
     As built: the chip is a click target using the shared painted geometry.
- [x] 4. Cover flat/grouped rendering, constrained width, filtering, the rail,
     same-frame toggles, and persisted restoration.
     Evidence: `crates/thegn-host/src/sidebar_view.rs` tests
     `layout_chip_names_the_layout_left_of_the_sort_chip`,
     `grouped_layout_chip_gives_way_before_the_sort_chip`,
     `flat_layout_chip_outlasts_the_sort_chip`, and
     `layout_chip_yields_the_header_row_to_the_rail_and_the_filter`;
     `crates/thegn-host/src/handlers/sidebar_mouse.rs` test
     `clicking_the_header_layout_chip_toggles_flat`;
     `crates/thegn-host/src/handlers/sidebar_persist.rs` test
     `load_reads_sidebar_flat`.
     As built: theme roles use accent/Faint through the theme chokepoint, and
     `model.sidebar_flat` is projected each frame; there is no separate theme
     test.
- [x] 5. Keep sidebar help explicit about `g` and complete focused rendering
     and OpenSpec validation.
     Evidence: `docs/help/sidebar.md` documents `g` and the clickable header
     chip. Focused validation is part of the batch precheck.
