# argv-templates

## ADDED Requirements

### Requirement: Templates preserve one argv element

The core tokenizer SHALL accept one caller-delimited UTF-8 argv element,
interpret only `{path}` tokens and doubled-brace escapes, and render exactly one
string. It MUST copy every other character literally, including shell quote and
escape characters, whitespace and NUL, and MUST NOT execute, split or re-parse a
token value.

#### Scenario: Shell syntax stays inert text

- **WHEN** an element contains quotes, backslashes, `$`, backticks, spaces or
  command-looking text outside a token
- **THEN** parsing and rendering preserve those characters as content and assign
  them no shell meaning

#### Scenario: A token value contains whitespace or braces

- **WHEN** a scalar token value contains whitespace, quotes or brace syntax
- **THEN** it is inserted verbatim into the same output element and is not split
  or re-parsed

### Requirement: Malformed and oversized elements fail closed

The tokenizer SHALL reject unbalanced braces, empty tokens, invalid token path
segments and unknown token paths with explicit errors. Template input and
rendered output MUST each be no more than `MAX_TEMPLATE_ELEMENT_BYTES` (16 KiB)
of UTF-8 bytes.

#### Scenario: Malformed token syntax

- **WHEN** a template contains an unmatched brace, an empty token or an invalid
  path segment
- **THEN** parsing returns an error and no command is produced

#### Scenario: An element exceeds the byte cap

- **WHEN** template input or token expansion exceeds 16 KiB
- **THEN** parsing or rendering returns `TemplateError::TooLarge` and never a
  truncated element

#### Scenario: Fixtures stay synthetic

- **WHEN** tokenizer behaviour is tested
- **THEN** the tests use synthetic templates and token sources and never execute
  a parsed template
