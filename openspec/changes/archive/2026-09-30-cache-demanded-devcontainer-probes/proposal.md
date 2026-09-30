# Demand-only bounded devcontainer capability diagnostics

THE-633: unrelated model hydration currently executes `devcontainer --version` once per build, even when no selected devcontainer can use that provider. Eligible diagnostics also repeat a subprocess unnecessarily, and the current probe waits before unbounded pipe reads.

Classify demand through the existing trust/status implementation, reuse a bounded capability result only for matching current command inputs, and reuse the owned bounded capture engine with an independent capability lane. Launch trust and `up` behavior remain separately governed. THE-639 tracks the remaining startup capture/custody defect.
