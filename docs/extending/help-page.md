# Add a help page

1. Create `docs/help/<slug>.md` with frontmatter: `id`, `title`, `order`,
   optional `parent`, `contexts: [zone:*, panel:*]` (which F1 contexts open
   it), `actions: [...]` (the action ids it documents). Unknown frontmatter
   keys are an error.
2. Add `include_str!("../../../../docs/help/<slug>.md")` to `SOURCES` in
   `crates/thegn-host/src/help/pages.rs`.
3. Mention every claimed action in the body. Write tables with plain pipes
   (the formatter aligns them; aligned pipes must still parse).
4. Never hand-write the keybindings or config-reference pages — they are
   generated.

**Gates:** `every_help_page_is_registered` (file ⇔ `SOURCES`),
`registry_validates_cleanly` (frontmatter), `page_action_claims_are_real_action_ids`,
`claimed_actions_are_mentioned_in_the_page_body`,
`every_panel_context_has_a_documentation_page` /
`every_zone_has_a_documentation_page`, `authored_tables_survive_the_formatter`.
