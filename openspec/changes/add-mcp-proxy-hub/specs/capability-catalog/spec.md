# Capability Catalog

## ADDED Requirements

### Requirement: Proxy control operations are catalog rows

The catalog SHALL carry `mcp_proxy.status` (read scope) and
`mcp_proxy.reload` (write scope), each mapped to its own control `Verb` and
projected on the `Cli`, `Http`, and `Grpc` surfaces per the catalog's
coverage contract. The tools of aggregated third-party upstreams MUST NOT be
minted as catalog rows — the catalog governs thegn's own capabilities; the
proxy's default-deny filter governs the third-party surface.

#### Scenario: Status reads, reload writes

- **WHEN** a control client with only `read` scope invokes the two proxy
  capabilities
- **THEN** `mcp_proxy.status` succeeds and `mcp_proxy.reload` is refused
  before any config re-read

#### Scenario: Upstream tools never enter the catalog

- **WHEN** the catalog tests run with proxy upstreams configured
- **THEN** no catalog row corresponds to an aggregated upstream tool
