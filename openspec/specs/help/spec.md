# help Specification

## Purpose

The in-app help (F1) is a registry of embedded pages with machine-checked coverage: every page is registered, every action and context is documented by a page that actually mentions it, and the generated pages (keybindings, config reference) are built from the same folds the CLI uses.

## Requirements

### Requirement: The help corpus is a registry

Every `docs/help/*.md` page SHALL be embedded via `help::pages::SOURCES` and every `SOURCES` entry SHALL exist on disk; page frontmatter (`id`, `title`, `order`, `parent`, `contexts`, `actions`) MUST reject unknown keys; the keybindings and config-reference pages MUST be generated at runtime and never hand-written.

#### Scenario: Orphan page

- **WHEN** a page is added to `docs/help/` without an `include_str!` in `SOURCES`
- **THEN** `every_help_page_is_registered` fails

### Requirement: Actions, zones and panel contexts are documented

Every bindable action id SHALL be claimed by some non-generated page's `actions:` list and MUST be mentioned in that page's body (by chord, id or a distinctive label word); every `zone:*` context SHALL be claimed by a page; every `panel:*` context SHALL be claimed or pinned in `test/help-context-ratchet.txt`. The action and prose allowlists (`test/help-ratchet.txt`, `test/help-prose-ratchet.txt`) are shrink-only.

#### Scenario: New action without a page

- **WHEN** an `ActionSpec` is added and no page claims its id
- **THEN** the action-docs ratchet fails

#### Scenario: Claimed but unwritten

- **WHEN** a page claims an id its body never mentions
- **THEN** the prose ratchet fails
