# Repair performance evidence and live log following

The live-build audit found that frame submission was described as a completed
flush, input dispatch was omitted from idle accounting, hydration child CPU was
unattributed, and a renamed log file stranded its live reader.

Repair those measurement boundaries and follow rotation without touching running
processes or changing performance policy. Preserve existing numeric metric keys
as compatibility fields, document their scope, and add independently counted
writer-completion and resync measurements. Controlled fixtures establish work
shape and measurement correctness; they do not claim a live speedup.

Impact: roadmap A.2/A.12 (native rendering and structured logging) and AW.718
(LogProvider). THE-614 coordinates the audit remediation; THE-625 tracks log following,
THE-626 telemetry correctness, and THE-627 controlled performance investigation. This change owns the
perf-suite/rendering/log-tail contracts; delivery registration is coordinated by
the integrating reviewer.
