# Usage Tracker

## ADDED Requirements

### Requirement: The usage overlay is compact and worst-first by default

The AI-account usage overlay SHALL default to one row per account — account
label, plan, the peak rate-limit window's gauge and used percent, and that
window's reset countdown — sorted by peak used percent descending, with
loading and unavailable accounts placed last and every gauge aligned in a
single column. The account label's tone SHALL reflect its peak window's
consumption tone, so the scan question — which account is closest to a limit —
is answered by reading order and color alone.

#### Scenario: Eight accounts scan in one screen

- **WHEN** the usage overlay opens with many configured accounts
- **THEN** each account occupies one aligned row, the account nearest its
  limit is first, and unavailable accounts sit at the bottom

#### Scenario: The hottest account is visually first

- **WHEN** one account's peak window crosses the warning threshold
- **THEN** that account sorts above healthy ones and its label carries the
  warning tone

### Requirement: Account detail is expandable in place

The usage overlay SHALL let the user expand a selected account to its full
detail — every rate-limit window with gauge and reset, trend sparklines with
exhaustion forecasts where history suffices, and the identity facts
(org, seat, tier, credential home) — and collapse it back. Identity facts MUST
render only in the expanded view, with the credential home abbreviated in
compact contexts, and the host-wide token rollup MUST stay a clearly labeled
separate block, collapsed to its totals by default.

#### Scenario: Expanding an account reveals the dense detail

- **WHEN** the user expands the selected account row
- **THEN** its full windows, trends, and identity facts render beneath it, and
  collapsing restores the compact row

#### Scenario: Two same-plan accounts stay distinguishable

- **WHEN** two accounts share a plan and organization
- **THEN** the expanded facts include the credential home, the last-resort
  discriminator

### Requirement: The usage snapshot is machine-readable across surfaces

thegn SHALL expose the usage tracker's per-account snapshot — accounts,
windows, identity facts, and states — as a read-scoped capability-catalog row
projected across the control surfaces, including a `thegn usage` CLI verb
whose default output is a plain aligned table and whose `--json` output is the
full payload via the one-emitter convention. The snapshot MUST read gathered
local state without initiating outward fetches and MUST NOT include credential
contents.

#### Scenario: Grepping the usage state

- **WHEN** `thegn usage --json` runs
- **THEN** every tracked account's windows, percentages, and reset times are
  emitted as JSON suitable for filtering and scripting

#### Scenario: Scope-gated over MCP

- **WHEN** the MCP server runs with an effective scope set lacking read
- **THEN** the usage snapshot tool is neither listed nor callable
