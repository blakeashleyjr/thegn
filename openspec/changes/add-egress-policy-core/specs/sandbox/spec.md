# Egress policy core

## ADDED Requirements

### Requirement: Host patterns have one grammar

Every egress host rule SHALL be parsed by one grammar: a bare `x.com` matches
the host and every subdomain, `*.x.com` matches strict subdomains only, `=x.com`
matches exactly that host, and `*` matches any host and stands alone. An entry
MAY carry a port suffix (`:443`, `:8000-8100`, `:*`); without one the policy's
ports apply. Names SHALL be normalised (trimmed, lowercased, one trailing dot
dropped, IDNA to A-labels) and the `*`, `=` and `_` markers and the allowed
character set SHALL be checked after percent-decoding and IDNA mapping. A name
outside letters, digits, hyphen and dot (plus `_` in non-registrable labels) MUST
be refused, as MUST an IP literal in any spelling. A pattern MUST keep its
meaning when written as its canonical string and parsed again. A block entry
without a port suffix MUST block every port of the host, whatever the policy's
default ports.

#### Scenario: Encoded or mapped markers are refused

- **WHEN** an entry is `%2a.example.com`, a fullwidth-asterisk `.example.com`,
  `ex!ample.com` or `ex(ample.com`
- **THEN** parsing fails instead of producing a pattern of another kind

#### Scenario: A portless block covers every port

- **WHEN** the allow list holds `x.com:8080` and the block list holds `x.com`
- **THEN** a request to `x.com` on port 8080 is denied

#### Scenario: A ported ceiling does not admit a portless entry

- **WHEN** a ceiling entry is `github.com:22` and a repo entry is `github.com`
- **THEN** the name-only clamp match does not admit the repo entry
