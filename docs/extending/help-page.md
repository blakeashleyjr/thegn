# Add a help page

The `{{keys:<section>}}` line in `docs/help/panel.md` expands from the panel's
section key tables when the help registry is built. Use it for a section's
key list instead of maintaining a second chord list. Git-family sections use
their `gitui::context_keys` table; other sections use `section_keys`.

`panel_doc_chords_are_section_keys` checks any remaining authored `Keys:`
lines against those tables. Its shrink-only debt list is
`test/help-section-keys-ratchet.txt`; `just help-ratchet-update` regenerates
it after prose changes. The generated Keybindings page folds the git context
tables as well as the ordinary section tables.

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
