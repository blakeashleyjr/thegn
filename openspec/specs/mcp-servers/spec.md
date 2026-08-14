# mcp-servers Specification

## Purpose

Declaring, acquiring (grant-checked), and inspecting user-configured MCP
servers; thegn emits them as a standard `mcpServers` settings block that a
user's coding agents can consume.

## Requirements

### Requirement: Users declare MCP servers agents consume

thegn SHALL let users declare MCP servers in config (`[mcp_servers.<name>]`)
with a launch command, arguments, and environment, and MUST emit them as a
standard `mcpServers` settings block (`thegn mcp emit`) that a coding agent's
settings can consume.

#### Scenario: A declared server appears in the settings block

- **WHEN** a `[mcp_servers.<name>]` is configured and the settings block is built
- **THEN** the block contains an `<name>` entry with its command, args, and env

### Requirement: MCP server binaries are acquired via the resolver, grant-checked

thegn SHALL acquire a declared server's binary through the shared managed-tool
resolver only when the server's declared grants permit that acquisition, and MUST
otherwise refuse with a clear message naming the missing grant. This applies when
a server specifies an acquisition `source` (npm / cargo / github-release).

#### Scenario: Granted acquisition proceeds

- **WHEN** `thegn mcp install <name>` runs for a server whose grants cover its
  source acquisition
- **THEN** the binary is acquired via the resolver and pinned

#### Scenario: Ungranted acquisition is refused

- **WHEN** a server's source acquisition is not covered by any declared grant
- **THEN** thegn refuses the install and names the missing capability

### Requirement: Declared servers and grants are inspectable

`thegn mcp list` SHALL list declared servers with their launch spec and grants,
`thegn mcp emit` SHALL print the `mcpServers` settings block, and `thegn
doctor` SHALL report declared servers and their grants.

#### Scenario: list and doctor surface declared servers

- **WHEN** MCP servers are declared and `thegn mcp list` (or `doctor`) runs
- **THEN** each server is shown with its command and its declared grants
