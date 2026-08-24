# help

## ADDED Requirements

### Requirement: Help pages resolve per locale with canonical fallback

The registry MAY embed per-locale page trees (`docs/help/<locale>/<page>.md`) beside the canonical English corpus; F1 lookup SHALL serve the active locale's page when embedded and fall back per page to the canonical one. All help ratchets and registry-coverage tests (action/prose/context ratchets, registered/orphan-page tests) SHALL evaluate only the canonical corpus — a translated tree can never satisfy or break a ratchet. The generated config-reference page SHALL remain English (it documents English config keys and the example config's own comments); the keybindings page's generated structure localizes with the chrome.

#### Scenario: Untranslated page falls back

- **WHEN** the active locale has a tree that lacks `sidebar.md`
- **THEN** F1's sidebar page renders the canonical English page

#### Scenario: Translations cannot move ratchets

- **WHEN** a translated page claims an action id or omits a chord the canonical page mentions
- **THEN** no help ratchet result changes
