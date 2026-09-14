# THE-611 static CLI startup boundary

Implementation candidate on `audit/maintenance-04-static-cli`, base7014a496. Source-only; native compilation, actual CLI executions, parent counterfactual and final lint/spec gates are pending. Root owns those gates and delivery.

`command_intent` exhaustively borrows all59 Command variants and each nested Config/API/Automations action. Only six StaticIntent forms return before migration/profile reroot. Existing helpers retain path spelling, schema source, API tables, grouped completion generation, basename and best-effort stdout. Schema derivation still constructs its declared defaults; the repaired boundary is no effective-config load/install or fabricated handler Config.

Parser fixtures call actual Cli::try_parse_from for all55 configured variants plus the four families and all nested variants; they assert parsed identity and exact intent. Five shells, two registration modes and both global argument positions are exercised without calling handlers.

The six actual-binary test functions (four portable plus two Unix) cover nine output forms, malformed/missing/FIFO inputs, invalid overrides, both profile selectors, exact private filesystem canaries, path spelling/default path, executable alias, help/version, EPIPE and configured loader controls. They never inherit stdin or PATH tools. The config-get control returns drawer.height="17", distinct from default"35%". No configured edit/setup/provider/daemon action is executed. FIFO negative controls remain source-traced observations, not proof that their configured action ran.

Private subprocess custody is reserved before spawn. File output has an8MiB accepted limit; success has a10-second deadline, configured FIFO observation500ms, termination a10-second polling budget. Drop tries bounded500ms cleanup; an unreaped child and its filesystem remain in one of32 static test custody slots. No detached thread, raw-PID fallback, or unbounded wait is used. Outer test-process custody is required for pathological system calls/retained failure. Private env paths are not an OS sandbox or universal no-home-access guarantee.

Primary source review and independent early production review are positive. Final fixture review and root execution remain pending. THE-505/592 checked admission, THE-607 worker loading, THE-612 source projections and existing dynamic TAB behavior are not closed by this repair.

Independent fixture review accepted the observation-error correction: any failed child observation permanently revokes further signal/wait syscalls and retains custody. A deterministic callback-count regression covers initial loss and loss after prior successful observation without spawning a child. Native execution remains pending.
