# Add an action (keybind / palette row)

1. **Variant**: add it to `pub enum Action` in
   `crates/thegn-host/src/keymap.rs`, with a doc comment.
2. **Id**: one arm each in `Action::key()` (`Action::X => "kebab-id"`) and
   `Action::from_key()` (`"kebab-id" => Action::X`; legacy aliases go here).
3. **Spec**: an `ActionSpec` in `crates/thegn-host/src/keymap_specs.rs` —
   `label`, `hint`, `default_chords` (may be empty), `palette`, and at least
   one search `keyword`. This is what the palette, `thegn keys list`, the
   generated keybindings page and the help ratchet see.
4. **Default chord (optional)**: `map.insert_all("Chord", Action::X)` in
   `default_keymap()`. Spell it exactly as in the spec.
5. **Dispatch**: an `Action::X => { … }` arm in the action match in
   `crates/thegn-host/src/run.rs` (or a handler in `src/handlers/` called
   from it). Never a string comparison in the palette Enter chain.
6. **Help**: claim the id in a `docs/help/<page>.md` frontmatter `actions:`
   list **and** mention it in the body (chord, id, or a distinctive label
   word).

**Gates:** `every_action_key_has_a_spec_and_round_trips` (variant without a
spec, or `key`/`from_key` disagree), `declared_default_chords_actually_dispatch`
(spec chord ≠ bound chord), `every_action_has_search_keywords`,
`every_palette_key_is_an_action`, `action_docs_ratchet` +
`claimed_actions_are_mentioned_in_the_page_body` (help), the `run.rs`
exhaustive match (compile error).
