# architecture-gates — delta for define-gui-frontend-lane

## ADDED Requirements

### Requirement: Graphical frontends are thin clients, enforced by dependency gates

GUI toolkit and window-system crates (`egui`, `eframe`, `iced`, `winit`, `wgpu`, `gpui`, `tauri`, `slint`, `druid`) SHALL have no owner crate in the crate-boundary test and SHALL be banned by `cargo deny`, so that no workspace crate — `thegn-host` above all — can link a windowing or GPU-widget stack. A future graphical frontend MUST arrive as a new client crate that speaks the version-pinned control wire (and, when published, the attach/frame wire) with a deliberate boundary-test entry naming it as that substrate's owner, and MUST hold no capability that is not a `capability::CATALOG` projection. The terminal UI SHALL remain the reference frontend: no capability may be exposed to a graphical frontend that the catalog does not expose to other surfaces.

#### Scenario: A GUI toolkit sneaks into the compositor

- **WHEN** `winit` (or any listed crate) is added to `crates/thegn-host/Cargo.toml`, directly or transitively
- **THEN** `just deps-audit` fails on the deny ban and `just test` fails the crate-boundary test naming the crate and the frontend-lane rule

#### Scenario: A sanctioned frontend crate is added later

- **WHEN** a new workspace member is declared as the owner of a GUI substrate in the crate-boundary test alongside a deny-ban exception scoped to that member
- **THEN** the gates pass for that member only, and `thegn-host` / `thegn-core` still reject the substrate
