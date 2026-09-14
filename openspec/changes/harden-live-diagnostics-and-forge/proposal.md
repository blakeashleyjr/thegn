# Harden live diagnostics and forge refresh

## Why

The September 13 live-build audit reproduced printable keyboard data in debug
logs, false connectivity recovery messages, repeated configuration warnings,
and native GitHub failures. Source review additionally reproduced an offline
reload that disables recovery, public GitHub requests for enterprise origins,
and a credential lookup outside the request timeout.

Parent: THE-614. This change addresses THE-619 (input), THE-620
(connectivity), THE-621 (forge host), THE-622 (typed errors), THE-623
(credential helper), and THE-624 (logging and warning repetition).

## What Changes

- Emit only safe key classes, modifier bits, and dispatch outcomes; never key
  or action payloads, even under broad DEBUG/TRACE directives.
- Update network policy without resetting evidence or recovery history; publish
  state consistently and distinguish initial connectivity from recovery.
- Restrict native GitHub to public GitHub origins; use the CLI for enterprise.
- Classify the pinned octocrab error variants and exercise real GraphQL error
  envelopes through an in-process mock service and the fallback ladder.
- Bound credential helper runtime and output, including inherited output pipes,
  and terminate the helper's process group on timeout or failure.
- Preserve default third-party log suppression across config reconciliation and
  preserve explicit user filters. Suppress repeated runtime config warnings
  using a bounded cache keyed by source/content and diagnostic fingerprints.

## Impact

Roadmap: A item 6 (forge/provider seams), AI items 749–750 (diagnostic surfacing
and coalescing), and remote/network status item 156. This completes bounded
reliability fixes, not those broader roadmap features. Capabilities: config,
forge, keybindings. No new config keys, network probes, runtime migrations,
telemetry, credential caching, or live-process operations. Repository access
still depends on the actual authorized account and is a separate operational
follow-up; this change does not claim to repair access to a removed or
unauthorized repository.
