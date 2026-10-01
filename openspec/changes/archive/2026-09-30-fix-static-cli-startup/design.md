# Static command admission

The private classifier borrows the parsed Command; exhaustive outer and Config/API/Automations matches make additions a compile-time review point. StaticIntent contains only existing printing arguments. SourceInspection and Recovery tags describe other actions and do not authorize any bypass.

Main returns through the static helper after Clap parsing and log-level argument composition, before migration, profile reroot and run_subcommand. Existing config/API handlers call the same printing functions. Completion still uses the invoked basename, grouped parser tree, buffered generation and best-effort stdout output.

No effective Config is loaded or installed, and no fabricated handler Config is passed. Schema derivation still invokes schemars' existing default-value generation; this is not a claim that Config::default or all environment reads disappear. Config::path retains existing platform config-home lookup and explicit paths are printed unchanged.

No render damage channel or wake path is touched. No SQLite schema change or user_version bump. Existing help commands retain their parser route; no new help context is introduced.

Actual-binary fixtures clear inherited environment, leave HOME absent, set private XDG/THEGN_DIR/platform roots, empty PATH and null stdin. This is fixture configuration, not filesystem isolation. File-backed output is size bounded. Each process reserves retained custody before spawn, polls a deadline and kills only its retained Child. Failed final reap holds both process and private TempDir in a bounded static test slot; no unbounded Drop join or cleanup thread. An outer owned test-process watchdog is still required for pathological OS/filesystem stalls and retained failure.

The regular-file config-get positive control uses a nondefault string value. FIFO configured siblings are observed blocked and terminated before their handler; they never dispatch an API or automation. Static FIFO success and unchanged full private-path snapshots establish the no-loader/no-startup-effect boundary. Windows lacks the Unix FIFO/pipe fixtures; portable output, migration/profile, alias and parser fixtures remain compiled/executed gates there.

Independent fixture review accepted the observation-error correction: any failed child observation permanently revokes further signal/wait syscalls and retains custody. A deterministic callback-count regression covers initial loss and loss after prior successful observation without spawning a child. Native execution remains pending.
