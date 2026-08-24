# cli

## ADDED Requirements

### Requirement: CLI output is locale-independent

Machine-consumed CLI surfaces SHALL NOT vary with `[ui] language` or the host locale: `--json` documents (via `cmd::emit_json`), exit codes, `thegn doctor` probe words, and `thegn keys list` output are byte-stable across locales; human-facing CLI prose remains English in this phase as deliberate policy (localization applies to TUI chrome only), so scripts written against thegn's CLI never break under a user's locale.

#### Scenario: JSON is byte-stable across locales

- **WHEN** `thegn wt list --json` runs with `[ui] language = "ja-JP"` and again with the default
- **THEN** the two documents are identical for identical state

#### Scenario: Doctor probe words are greppable

- **WHEN** `thegn doctor` runs under a non-English locale
- **THEN** probe result words (`present`, `absent`, …) render in English exactly as documented
