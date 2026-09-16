**Independent review: input logging, forge refresh, diagnostic noise, and connectivity**

Reviewed September 13, 2026 against `f4c1355b` and the current working tree. This reviews findings 2, 3, 7, and 8 of [the operational audit](live-build-2026-09-13.md). The existing `justfile` modification was preserved. No production code, configuration, runtime state, processes, credentials, or external tickets were changed.

All four original findings are supported. Connectivity deserves higher priority than the original report suggests: an unchanged configuration load while offline can disable the recovery probe while leaving background refreshes paused. Additional source defects affect enterprise forge routing, default log-filter reconciliation, and the bounds of credential lookup.

**Independent evidence check**

I rescanned the original host run `tlbp111dis` through **16:37:06 local time**, counting event classes without printing or reconstructing typed input. Rotation has moved the original evidence into `thegn.log.1`; references are valid as of this review.

| Observation                            |                 Independently reproduced | First relocated record |
| -------------------------------------- | ---------------------------------------: | ---------------------- |
| Key dispatch DEBUG events              | 139, including 127 with character fields | `thegn.log.1:54310`    |
| Native forge request failures          |                                      554 | `thegn.log.1:54477`    |
| `workspaces_dir` compatibility notices |                                    1,263 | `thegn.log.1:54124`    |
| `workspace.cms` compatibility notices  |                                    1,263 | `thegn.log.1:54125`    |
| Unknown extension/MIME warnings        |                                      478 | `thegn.log.1:54264`    |
| “Network back online” messages         |           29, with zero offline messages | `thegn.log.1:54408`    |

The counts and relocated references are saved in `/tmp/thegn-logging-review-counts.json`. They deliberately use the original cutoff, rather than combining a longer live run with the earlier audit's denominators. The historical daemon burst, database PR-cache timestamp, and selected GitHub credential were not independently revalidated here.

**1. High — input diagnostics preserve printable characters. Original finding 2 confirmed.**

[run.rs:19917](../../crates/thegn-host/src/run.rs) checks whether DEBUG is enabled for `thegn::input`, then writes `raw_key` and `norm_key` with their Debug representations. There is no separate consent or diagnostic flag for printable input. [justfile:1093](../../justfile) defaults `live` to DEBUG and [justfile:1111](../../justfile) supplies that broad filter. The comment claiming this is free unless the specific input target is enabled is misleading: a global DEBUG directive also enables it. The file formatter forwards event fields directly at [log_trace.rs:582](../../crates/thegn-core/src/log_trace.rs).

The observed 127 records with character fields confirm actual recording, not merely a hypothetical log call. Echo suppression by a child application does not affect this host-side logging path. No credential exposure to another person, credential entry, or reconstructed input sequence was established. This call site predates the reviewed build interval.

Remediation: make ordinary character values absent from default diagnostics at the event source. Keep key class, modifiers, and action outcome; gate any character-level debugging behind an explicit opt-in. Adding `thegn::input=info` to launch recipes is useful defense in depth but does not protect users who set another broad DEBUG filter. Existing support-bundle redaction recognizes sensitive key names, including the `_key` suffix; this review does **not** claim that bundle export necessarily preserves the same fields as raw disk logs.

Meaningful regression test: capture emitted events for printable input with no action under the exact `just live` filter, with a broad TRACE filter, and with any proposed opt-in. Assert the characters are absent by default while modifier/action diagnostics still appear. Include input forwarded to an echo-disabled child; do not use real credentials.

**2. High — config loading can strand the app offline. Original finding 8 confirmed and expanded.**

[Config::post_process at config.rs:6072](../../crates/thegn-core/src/config.rs) invokes [NetworkConfig::install at config_network.rs:65](../../crates/thegn-core/src/config_network.rs) on every load. This occurs during ordinary background hydration through [hydrate.rs:4244](../../crates/thegn-host/src/hydrate.rs), not just when the user edits configuration. [install_thresholds at connectivity.rs:274](../../crates/thegn-core/src/connectivity.rs) replaces the entire mutex-protected state with a new `Unknown` state, losing failures and probe history.

There is a more serious consistency error: this replacement does **not** update the separate `HOT` atomic read by [current at connectivity.rs:183](../../crates/thegn-core/src/connectivity.rs). After reaching Offline, loading unchanged configuration leaves `current() == Offline` but resets the internal machine to Unknown. [ConnState::should_probe at connectivity.rs:117](../../crates/thegn-core/src/connectivity.rs) returns false for Unknown. The ticker requires both `is_offline()` and `should_probe()` at [hydrate.rs:773](../../crates/thegn-host/src/hydrate.rs), so automatic recovery stops being scheduled while [connectivity_gate.rs:77](../../crates/thegn-host/src/connectivity_gate.rs) continues to suppress normal PR, issue, CI, and fetch backstops.

An isolated executable including the **actual source module** reproduced:

```text
three failures -> current Offline; failures 3; recovery probe available
install identical thresholds -> current Offline; failures 0; recovery probe unavailable
independent success -> current Online
```

The harness is `/tmp/thegn-connectivity-review.rs`; only the two tracing macros were replaced with no-ops. Its assertions passed. All **nine existing connectivity unit tests also passed** in an isolated single-threaded test binary, demonstrating that the existing tests miss policy-reinstallation behavior. There is no evidence that the live run entered this trapped state: the captured window contains no offline transitions. A manual or otherwise ungated successful network call can still recover the holder, so this is not an assertion that the entire app becomes permanently offline.

The original repeated recovery-message finding also stands: Unknown→Online is logged as “back online,” and repeated hydration loads recreate Unknown. Those 29 messages do not show 29 real outages.

Remediation: separate parsing/resolution from installation of process-global policy, and update thresholds without resetting observed state, failure streak, or probe timestamps. Define behavior for actual threshold changes and forced-mode transitions, preserving consistency between the atomic and mutex representations.

Meaningful regression tests: reinstall identical policy while Online, after two failures, and while Offline; assert state/history/probe cadence and transition count remain correct. Test changed thresholds and Offline→Auto mode explicitly. Run global-holder tests in isolated processes or serialize them; the current configuration test explicitly avoids assertions because other tests mutate the same globals.

**3. High — native GraphQL failures bypass the documented CLI fallback. Original finding 3 confirmed and sharpened.**

[native.rs:315](../../crates/thegn-svc/src/forge/native.rs) expects GraphQL `errors` to be present in an `Ok(Value)` response. However, the pinned dependency is **octocrab 0.54.1** ([Cargo.lock:5777](../../Cargo.lock)). Its locally installed `src/lib.rs:1456–1470` converts a GraphQL error envelope into `octocrab::Error::Graphql` and returns only `data` on success. Consequently the branch promising CLI fallback does not handle normal GraphQL error envelopes, including partial successes containing errors.

Those responses enter [native.rs:329](../../crates/thegn-svc/src/forge/native.rs), where a display-string search for `connect`, `dns`, or `tls` decides between Offline and Other. The captured “could not resolve repository” shape becomes Other, which is final in the ladder. This is a deterministic mismatch with the pinned API, rather than an intermittent fallback problem.

The string heuristic has a related defect: repository names or other server-provided GraphQL error text containing `connect` can be mistaken for transport failure and feed the **global** offline detector. Conversely HTTP authentication/rate-limit responses are collapsed to Other despite available `NotAuthenticated` and `RateLimited` variants. Repository-specific authorization failures should neither be treated as successful refreshes nor poison global connectivity.

Token precedence is confirmed from [native.rs:23](../../crates/thegn-svc/src/forge/native.rs): nonempty `GH_TOKEN`, then `GITHUB_TOKEN`, then `gh auth token`. This review did not read or print the live process's token, identify its account, or access GitHub. A CLI fallback often uses the same environment token, so repairing classification alone is **not proof** that the Sage repository will become accessible. A renamed, removed, or unauthorized repository remains a possible operational cause. The original 554 failures and repo-specific diagnosis remain valid.

Remediation: classify typed octocrab variants/statuses, put actual GraphQL failures through the intended fallback policy, and bound/back off persistent errors per repository. Keep last-known data while exposing the failed refresh and its age; the PR-list refresh currently handles only `Ok` at [hydrate.rs:3907](../../crates/thegn-host/src/hydrate.rs). Verify the exact repository under the selected host/account without logging the token.

Meaningful tests: a local mock transport returning HTTP 200 with `errors`, partial `data` plus `errors`, 401, rate-limited 403/429, 5xx, and actual transport timeout. Count native/CLI calls at the ladder boundary and assert connectivity effects. Include an error whose repository name contains `connect`. Parser-only fixtures and existing ladder tests for manually constructed errors do not exercise this dependency boundary.

**4. High for affected repositories — native forge routing ignores the origin host. Additional confirmed source defect.**

[forge/mod.rs:235](../../crates/thegn-svc/src/forge/mod.rs) installs the same `GithubNative` ahead of the CLI for **both** GitHub and GitHub Enterprise. The enterprise flag is passed only to the CLI. The native gate obtains owner/repository through [native.rs:181](../../crates/thegn-svc/src/forge/native.rs), delegating to the deliberately host-agnostic [model.rs:314](../../crates/thegn-core/src/forge/model.rs). It never validates the GitHub hostname. The octocrab builder at [native.rs:307](../../crates/thegn-svc/src/forge/native.rs) never sets `base_uri`; the pinned dependency defaults to `https://api.github.com`.

Thus an enterprise origin can query the public GitHub repository with the same owner/name. A successful native answer wins before the correctly configured CLI gets a chance; a missing public repository hits finding 3. With no explicit forge entries, the default ladder is also used for other origin hosts. A fixture extracted from the unchanged parser functions confirmed identical accepted owner/name values for public GitHub, enterprise, and non-GitHub origins (`/tmp/thegn-forge-parser-review.rs`). No network request was made.

This is confirmed routing behavior, **not** evidence that the current Sage failure involves enterprise GitHub; that reported origin is on public GitHub.

Remediation: restrict the current native implementation to supported public GitHub origins and fall through for other hosts, or carry the selected forge host through both the HTTP base URI and host-scoped credential lookup. Test both a same-name repository existing on public GitHub and one absent there; assert the wrong host is never contacted.

**5. Medium — diagnostic noise is re-enabled by both live filters and ordinary startup reconciliation. Original finding 7 confirmed and expanded.**

The fixed-window counts reproduce the original **2,526 compatibility warnings plus 478 MIME warnings**, about 84% of its 3,587 warnings. [config.rs:5880](../../crates/thegn-core/src/config.rs) and [config.rs:5975](../../crates/thegn-core/src/config.rs) emit compatibility diagnostics on every load/overlay without deduplication. Background hydration invokes the loader frequently, so editing the config file is not required for repetition.

[log_trace.rs:175](../../crates/thegn-core/src/log_trace.rs) intentionally supplies `log=error` in the initial default filter; an explicit `THEGN_LOG` replaces it, explaining the `just live` noise. Additionally, [reload_level at log_trace.rs:201](../../crates/thegn-core/src/log_trace.rs) constructs a filter from **only** the level. [run.rs:652](../../crates/thegn-host/src/run.rs) calls this at startup when neither logging environment variable is present. Consequently an ordinary configured file sink also loses the noise-suppression directive during startup reconciliation. Fixing only the live recipe leaves this path broken. The WARN ring has its own fixed `warn,log=error` filter and is not changed by `reload_level`.

Remediation: preserve baseline directives when reconciling config levels and in the default live filter, while defining how an explicit user override works. Deduplicate compatibility warnings by source/content/diagnostic, allowing genuinely changed configuration to produce fresh diagnostics. Keep raw log retention calculations separate from claims about the in-memory ring.

Meaningful tests: install a file sink without logging environment overrides, emit a bridged WARN and an application WARN before/after `reload_level`, and verify that only the intended application warning survives both times. Repeatedly load identical legacy configuration, then change one source and confirm warning emission follows the chosen policy. This finding is source-reviewed; no live filter was modified.

**6. Medium — the advertised native request timeout does not bound credential lookup. Additional confirmed source defect.**

When no environment token exists, every native operation executes [gh_auth_token at native.rs:426](../../crates/thegn-svc/src/forge/native.rs), using an unbounded `Command::output()`. That runs inside `gate` **before** the ten-second `tokio::time::timeout` in `graphql`. A hanging credential-helper/keyring/CLI path can therefore hold a background worker beyond the advertised request timeout; successful lookups also spawn `gh auth token` for each of the paired PR status/list refreshes. No hanging credential helper was observed in the live logs, and the live credential source is unknown.

Remediation: bound the credential subprocess and consider host/account-aware token caching with explicit invalidation. A fake `gh` that sleeps beyond the bound is the meaningful isolated test; assert the refresh returns and the child is reaped without involving real credentials.

**Review limits and priority**

No full release build, application suite, live restart, external authentication probe, or network mock test was run. Executed checks were the fixed-window log recount, the actual-source connectivity reproduction, nine existing connectivity unit tests, and the extracted native-origin parser fixture. Other test descriptions above are proposed regression coverage, not claimed passes.

Prioritize printable-input logging and connectivity state preservation, then typed GraphQL classification and host-correct routing. The live repository access problem still requires verifying the actual selected credential against the exact repository. Noise suppression and bounded credential lookup should accompany those fixes rather than being treated as resolved by a successful launch.
