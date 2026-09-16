# THE-321 + THE-322 combined adversarial source review

- Candidate: `d96c91faabafed34a5fe4d4e3222ee3c1d981e7a` (requested frozen source was `7ef9c3c6c0ab5637fc913e1b1b94a0d6a799cfa0`; reviewed head includes the ratchet-only `7a2136ef` and `d96` follow-ups)
- Base: `fadd7164ce46b684e1e23ee2174aa876fc619d42`
- Candidate tree: `32c1ebc922c7f35bfee7bbc7c048d6ac35553a0b`
- Verdict: **BLOCKED before compiled gates**
- Scope: source/evidence inspection only; no Cargo, build, test, native, provider, Linear, or canonical operation.

The first blocker is a compile-shape error in `crates/thegn-svc/src/issue/kaneo.rs:774-783`. `task_to_domain` is declared at lines 360-365 as returning `Result<Issue, IssueError>`. `update_issue` wraps that value in `Ok(task_to_domain(...))`, unlike the corrected create/get call sites, so the boxed future resolves to a nested `Result` instead of the trait’s required `Result<Issue, IssueError>`. This must be corrected before paying for compilation.

The main production security blocker is in `crates/thegn-svc/src/issue/linear.rs:595-610` and `621-629`: `StateNode.id` comes from the provider response and is inserted verbatim into the follow-up `issueUpdate` GraphQL document. A quote, backslash, or GraphQL punctuation in that response can change the mutation sent with the Linear token. User-controlled title and identifier values are escaped, but this response-controlled value is not. Use a GraphQL variable or validate/escape the state id and add a response-injection fixture.

GitHub creation still has an authority/side-effect gap at `github.rs:390-420`. `gh issue create` receives no repository flag and runs in the backend directory; the printed URL is checked only after creation. `extra_flags` are documented as `gh issue list` flags, so configured list scope cannot be assumed to bind create. A cwd or host mismatch can create in the wrong repository or create successfully before returning a host/route parse error. The nearby comment says creation never falls back to cwd, which does not match the argv. Bind creation before the side effect or document and test the cwd contract explicitly.

Linear’s `linear_issue_to_domain` at `linear.rs:307-329` constructs ids and carries the response URL before validation. Router methods validate later, but a direct consumer of the public `LinearBackend` can receive malformed provider data. Make this conversion fallible and admit response identity/URL before domain construction, as Jira and Kaneo do.

The merged API also changes public signatures: `PluginIssueBackend::new` now returns `Result<Self, IssueError>` (`plugin/provider.rs:198-212`) and `IssueRouter::push_backend` now returns `Result<(), IssueError>` (`issue/mod.rs:429-445`). Existing downstream callers of the old public signatures stop compiling; adding `IssueError` variants similarly affects exhaustive matches. Preserve compatibility with fallible companion methods or explicitly version the breaking change.

Source inspection otherwise found the intended shared HTTP permit/deadline, origin/base-path, bounded body, structured Kaneo route/query, provider identity, router/cache, and control one-segment behavior in place. Root-reported rustfmt/OpenSpec/diff ratchets pass; no compile or runtime result is claimed here. The `7a2136ef` brand-guard change is a narrowly scoped, reasoned allowlist for two immutable historical receipt paths, and `d96` only tightens fixture result/cancellation assertions relative to `7ef`.

Exact reviewed source hashes are in the companion JSON report.
