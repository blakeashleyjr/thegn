# THE-322 Linear delta independent adversarial review

Date: 2026-09-15
Candidate: `/tmp/thegn-THE322-linear-revision-luna-20260915`
Base: `d96c91faabafed34a5fe4d4e3222ee3c1d981e7a`
Candidate commit: `0ddde12c83b165ed03f775e9419b229c56a423db`
Candidate tree: `fcbff499b1becade78d4c8d7277f93a37580ddca`

Verdict: **source approved for owner compile/test gates; no blocker found in this Linear-only delta**.

The delta is limited to `crates/thegn-svc/src/issue/linear.rs`. `linear_issue_to_domain` is now fallible and validates the provider identifier, public URL, and completed issue identity before returning an issue. List and search collect `Result<Issue, IssueError>` values, while get/create/update propagate conversion failures with `?`/`transpose`; no response conversion error is silently discarded. The existing constructor remains source-compatible, with only the test injection constructor and existing budget constructor used for fixtures.

The workflow-state response is admitted through the existing built-in segment policy before being escaped by the existing GraphQL string helper. The source-only hostile Axum fixture returns `state\";title:\"injected`, captures the second mutation document, and checks escaped quotes are present; malformed path/control/oversized values are refused. The fixture settles its server task after assertions. This validates the intended source boundary; it is not an external provider test.

The direct response tests cover malformed Linear identifiers and non-public URLs. The normal field and missing-optional fixtures use valid public URLs. The returned issue identity is checked after construction, so malformed provider data cannot enter the domain result or cache/router path.

Source evidence:

- `git diff --check` and the candidate's `rustfmt --edition 2024` check were recorded as passing.
- The delta has no changes outside `linear.rs`.
- Source search confirms every `linear_issue_to_domain` callsite propagates its `Result`.
- No Cargo, build, test, native/provider, authenticated Linear write, canonical checkout, or Linear mutation was run.

Integrated follow-up readback: root's `cf978191` adds the separately scoped five-line THE-322 identity correction (lifetime elision plus rejection of option-like GitHub repo operands and focused grammar assertions). It does not alter `linear.rs`; source inspection found no regression from that delta. The six-line commit remains additive to the reviewed Linear candidate.

The full combined tree still requires the owner's compile/test gates and review of unrelated provider changes.
