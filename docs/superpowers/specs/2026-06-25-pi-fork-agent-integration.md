# First-Party Agent as a pi Fork (termite-agent)

Date: 2026-06-25
Status: Planned / Transitioning

## Context & The Pivot

Between June 22 and June 24, 2026, the `superzej` architecture underwent a massive shift. We initially built `termite-agent` as an embedded, in-process Rust harness (`AgentUi` via `spawn_blocking`). However, as we formalized the **Two-Layer Control Plane** (`szproxy` for LLM traffic + ACP for agent conversation), it became clear that coupling the agent to the compositor in Rust was an anti-pattern.

We stripped the embedded `agent` tab and formally moved `termite-agent` to be a **`pi` fork** (a Node.js/TypeScript CLI).

This gives us the best of both worlds:

1. **The Brain (`pi` fork):** We inherit `pi`'s rich ecosystem—TypeScript extensions, `AGENTS.md` context file management, prompt templates, and the npm/git package ecosystem. We don't have to rebuild an extensible agent harness from scratch in Rust.
2. **The Hands & Bouncer (`superzej`):** By deeply integrating the `pi` fork over our **Two-Layer Control Plane**, the agent _feels_ embedded. It runs safely in our sandboxes, renders diffs in our native UI, and routes traffic safely through our proxy.

## How it embeds deeply via the Two-Layer Control Plane

### 1. The Upper Plane (ACP Client & MCP-over-ACP)

Instead of `pi` executing terminal commands and filesystem edits directly on the host, it will use **ACP** to delegate side-effects to `superzej`.

- **Sandboxed Execution:** `superzej` implements `terminal/create` and `terminal/output` over ACP. When the `pi` fork wants to run a bash command, it sends an ACP tool call. `superzej` executes it inside the active worktree's container sandbox (`podman`/`bwrap`/`none`), enforcing the policy boundary that `pi` intentionally omits.
- **Native Diff & Review Pane:** Instead of `pi` directly writing to files, its `edit` and `write` tool calls are intercepted via ACP and piped into superzej's native diff/review pane (`panel/staging.rs`). The user gets a visual side-by-side or unified diff, and can approve, reject, or request changes with one key.
- **House Tools via MCP-over-ACP:** We expose superzej's heavy-lifting native tools—like `sem` (semantic git), `weave` (semantic merge), and `rtk` (token reduction)—as an MCP server over the ACP channel. The `pi` fork gains these capabilities instantly without needing them compiled into its Node runtime.

### 2. The Lower Plane (`szproxy` & Environment Bundles)

We don't need to teach the `pi` fork about budget caps, token compression, or failover.

- **Virtual Keys:** Using the newly designed Environment Bundles (AU 684-697), `superzej` mints a per-worktree virtual key and injects it into the `pi` fork's environment when spawning it (`OPENAI_API_KEY=szk-...`, `OPENAI_BASE_URL=http://localhost:<proxy_port>/v1`).
- **Transparent Governance:** All LLM traffic from the `pi` fork routes through `szproxy`. It automatically inherits sequential failover, token usage attribution (tied back to the specific worktree), and strict budget caps. If the agent runs away, `szproxy` cuts it off.

## Future distribution (R2)

Because `termite-agent` is an external `pi` fork speaking ACP, it can be consumed by _any_ ACP client (e.g., Zed), not just `superzej`. We expose our `termite-agent` outwards (R2) while consuming it optimally inwards (R1 & R3).
