# terminal-search Specification

## Purpose

Terminal history search: literal, regex and fuzzy matching with exact-case and
whole-word controls, in-memory recall of recent queries, and feedback that
explains empty or capped result sets.

## Requirements

### Requirement: Query controls and session history

Terminal history search SHALL expose literal, regex and fuzzy matching plus
exact-case and whole-word controls against its existing search model. `Alt+c`,
`Alt+w`, `Alt+r` and `Alt+f` SHALL toggle those controls without leaving the
overlay, and the header SHALL show each active control as an `Aa`, `ab`, `.*`
or `~` chip beside the scope. Exact-case and whole-word SHALL apply to literal
and regex matching and be ignored by fuzzy matching. An invalid regular
expression SHALL show an inline error instead of an empty result list. Recent
non-empty queries SHALL remain available in memory for the current session and
be recallable with `Ctrl+r`.

#### Scenario: Regex matches only matching history lines

- **WHEN** the user enables regex mode and searches `err(or)?:`
- **THEN** only lines matching that expression are listed

#### Scenario: Whole-word matching excludes larger words

- **WHEN** the user enables whole-word mode and searches `log`
- **THEN** a line containing `login` without a standalone `log` is not listed

#### Scenario: Active controls are visible

- **WHEN** the user toggles exact case, whole word, regex or fuzzy
- **THEN** the matching `Aa`, `ab`, `.*` or `~` chip appears in the header
  until the control is toggled off

#### Scenario: Invalid expression is reported

- **WHEN** the query is not a valid regular expression
- **THEN** the overlay shows an inline regex error instead of an empty-results
  state

#### Scenario: Recent query is recalled

- **WHEN** the user reopens terminal search and presses `Ctrl+r`
- **THEN** the most recent non-empty query from this session is restored

### Requirement: Search feedback explains empty and capped results

A pane-scoped search with no matches SHALL suggest widening the scope with
`Tab`. When the result limit is reached, the footer SHALL say there are too
many results and suggest narrowing the query or scope.

#### Scenario: Pane search has no matches

- **WHEN** no lines match in the pane scope
- **THEN** the empty state suggests using `Tab` to widen the scope

#### Scenario: Result limit is reached

- **WHEN** matching history exceeds the configured result limit
- **THEN** the footer says there are too many results and suggests narrowing
  the query or scope
