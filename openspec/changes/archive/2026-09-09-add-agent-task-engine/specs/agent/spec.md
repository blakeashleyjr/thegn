# Agent

## ADDED Requirements

### Requirement: A configured agent can be resolved to a headless command

thegn SHALL let a background task name a configured `[[agents]]`/`[[tools]]`
entry instead of restating a command line, and SHALL resolve that name to a
non-interactive command that accepts a task prompt as an argument. Resolution
MUST derive the entry's provider from its explicit `provider` field or, absent
one, from its command's program basename, and MUST fall back to appending the
prompt as an argument (with a config warning) for a provider it does not
recognize, so an unrecognized agent still runs rather than being refused. An
explicit command template MUST take precedence over a named agent, so any agent
remains configurable regardless of what thegn knows about it.

#### Scenario: A named agent resolves to its headless form

- **WHEN** a background task is configured with `agent = "claude"` and no command
  template, and `[[agents]]` declares an entry named `claude`
- **THEN** the task runs that entry's program with its provider's non-interactive
  flags and the rendered prompt as an argument

#### Scenario: An unrecognized provider still runs

- **WHEN** the named entry's provider is one thegn has no headless flags for
- **THEN** the entry's command is run with the prompt appended as an argument and
  a configuration warning is recorded, rather than the task being skipped

#### Scenario: An explicit command template wins

- **WHEN** both a command template and a named agent are configured
- **THEN** the command template is used verbatim and the named agent is ignored

#### Scenario: Neither configured means no agent

- **WHEN** neither a command template nor a named agent is configured
- **THEN** no agent is dispatched and the task falls back to notifying
