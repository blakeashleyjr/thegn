# Tasks — plugin UI extension decisions (THE-107)

- [ ] Inventory enum vocabulary, loader support, render/input paths, config,
      docs, schemas, and tests for SidebarTab, Theme, and key zones.
- [ ] Evaluate each surface against the common identity, negotiation, permission,
      ownership, placement, budget, degradation, help, and compatibility gate.
- [ ] Decide adopt/defer/reject for SidebarTab; if adopted, define its bounded
      row/view model rather than arbitrary UI.
- [ ] Decide adopt/defer/reject for Theme; compare a plugin provider with existing
      safe user-theme files and define token/contrast/persistence constraints.
- [ ] Decide adopt/defer/reject for plugin key zones; default to focused-surface
      actions and host-owned conflict/dispatch/help behavior.
- [ ] Publish the decisions in the plugin support matrix and ensure reserved/
      absent surfaces remain rejected by negotiation.
- [ ] Create one separate implementation change per adopted surface with code,
      schema, config, docs, compatibility, security, and test acceptance.
- [ ] Verify no task/spec duplicates THE-108 PanelSection runtime or THE-106
      general contract-alignment scope.
- [ ] Run strict OpenSpec and documentation/schema consistency gates.
