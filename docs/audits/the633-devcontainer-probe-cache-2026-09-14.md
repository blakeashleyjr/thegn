# THE-633: demand-only devcontainer capability probes

Candidate source is in the isolated maintenance acceptance worktree, based on main `7014a496`. This report records source scope and pending gates; it does not claim tests have run.

`hydrate::collect_sidebar_status` previously invoked `devcontainer_provider::probe` before examining any worktree. The existing controlled actual `build_model` workload records nine immediate fake-helper executions for nine unrelated hydration samples. The repair asks the existing status classifier to request a capability report lazily, then memoizes it within that build and uses a bounded demand-only cache across builds.

The cache uses the actual inherited command environment, cwd and executable identity. It retains only a digest of environment values, follows ordinary executable symlinks, validates the opened descriptor as regular, and rejects oversized or unverifiable input snapshots. Ready results expire after 30 seconds; unavailable/degraded results after 5 seconds. Installation, PATH/input changes and replacement trigger fresh discovery without waiting for TTL. Generation tokens refuse stale and A→B→A completion, and Condvar waiters recapture their own inputs. There is no timer or detached cache producer.

Capability capture shares the existing private bounded engine with Git, but has a separate one-slot counter and retained reaper/queue, a maximum of two seconds and 16 KiB per stream. Git keeps two slots, 15 seconds, 2 MiB per stream and its original nonzero-refusal policy. The capability layer returns the real status plus both output streams so the provider preserves nonzero version diagnostics. Late readers/children retain their lane budget; a blocked capability reaper cannot block Git reaping.

Source revisions from review:

- Independent review found `same_file::Handle::from_path` could block on a FIFO swapped after `is_file`. Platform identity opening now uses Unix nonblocking flags (Windows metadata-only access) and descriptor regular-file validation before conversion. The FIFO fixture includes an owned rescue descriptor so the counterfactual blocking implementation fails without stranding its reader.
- The provider's entire lazy refusal precedence stays in the original classifier; source trust, non-source pending requests, user-pinned source, sandbox policy, blocked substitutions and recognized field disposition are not duplicated in cache code.
- Cache-key construction has explicit entry/count/aggregate/path bounds. Standard-library environment snapshot allocation and OS filesystem latency are not hard realtime guarantees.
- Admission tests assert exact refusal categories, preventing ordinary spawn failure from satisfying an invalid-policy test accidentally.

Regression fixtures added, execution pending:

- Seven cache tests cover precise TTL edges, unavailable→installed, changed environment/cwd/PATH, equal-size/equal-mtime replacement while the old handle is pinned, actual Condvar coalescing, waiter reobservation, stale and ABA completion, unwind/poison recovery and bounded input refusal.
- One POSIX fixture runs the actual cache→captured environment/cwd→provider→bounded subprocess path. It proves one invocation across repeated demand and preserves nonzero status/version diagnostics.
- Two classifier tests cover no selection, disabled/uncontained, malformed/ambiguous selection, source/non-source approvals, source precedence, sandbox policy, blocked environment expansion, refused/reserved/unknown fields and the eligible positive control.
- Seven capture tests cover exact pre-spawn admission categories, fixed lane limits, full nonzero stdout/stderr, each stream's overflow, a hung owned child, retained inherited-pipe capacity while Git still captures, and a blocked capability reaper while Git reaping completes.
- Two Unix identity tests cover FIFO replacement refusal and an ordinary executable symlink positive control.
- The existing ignored `platform::unix::perf_workloads_hydration::controlled_full_hydration_workload` now asserts zero probes for its unrelated worktrees. Run the same release workload at `THEGN_AUDIT_WORKTREES=1`, `8`, and `32`, serialized with process-sampler measurements. It creates private Git/DB fixtures and a private PATH containing git/sh plus an optional immediate fake devcontainer; no real provider is invoked. HOME is never changed. This proves owned fixture behavior, not absence of every possible read of host-home configuration.

Pending landing gates: actual host fixture compilation/execution, existing Git and provider regression tests, final primary/independent review, controlled before/after measurements, and integrated configured/lint gates. Native Windows and Darwin execution are not established by Linux tests; any cross-check must be reported separately.

Remaining scope: this cache is diagnostic coherence, not atomic executable attestation against hostile same-UID modification. In-place changes that restore identity metadata can remain undetected until a later demanded TTL refresh. OS spawn/filesystem calls themselves are not interruptible.

[THE-639](https://linear.app/blakeashley/issue/THE-639/bound-devcontainer-startup-output-and-retain-child-ownership-through) tracks the separate startup/up capture and custody defect. In `devcontainer_provider.rs`, `CliProvider::start` still calls `run_bounded` (currently line 526); that helper (currently line 667) waits for exit before `read_to_end`, has no output limit, and performs a blocking wait on timeout. Those lifecycle operations are deliberately outside THE-633's version-probe repair.
