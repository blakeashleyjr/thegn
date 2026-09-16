# Combined THE-321/THE-322 compile-correction delta — independent Luna review

Candidate: `/tmp/thegn-maintenance08-combined-20260915`  
Base: `cf978191`  
Reviewed commit: `7a36ab0316e050a3056e72eabb8c44939306d326`  
Tree: `6166d6b5bab341157244e782122c7652e712b635`

Verdict: **approve the corrected delta for the warm compile**. No source blocker remains in the three corrections represented by this commit.

The pre-fix diagnostics in `/tmp/thegn-maintenance08-clippy-20260915.log` report eight library errors plus one test error. This delta addresses their causes:

- `JiraBackend` now retains `base_url` and initializes it in its constructor. The existing five response conversion call sites that pass `&self.base_url` therefore have a field-backed reference, with no additional struct literal found in the service sources.
- `Kaneo::list_issues_with_op` is in an inherent `impl KaneoBackend`, while the `IssueBackend` implementation retains only the trait methods and calls the helper from both `list_issues` and `search`.
- The Kaneo board fixture now calls `crate::issue::validate_issue_identity`, matching the function's actual module and existing provider usage.

The commit changes only `crates/thegn-svc/src/issue/jira.rs` and `crates/thegn-svc/src/issue/kaneo.rs`; `git diff --check` passes. I found no duplicate helper or remaining direct `JiraBackend`/`KaneoBackend` struct literal in the service source during this review.

The diagnostics file is from the pre-fix compile snapshot, so a corrected warm compile is still the gate that can reveal unrelated errors outside this exact delta. I did not run Cargo, builds, tests, native/provider execution, or mutate canonical/main.

## Hashes

- `crates/thegn-svc/src/issue/jira.rs`: `69abd007d02ab850415c6a82fe59bc8b71b30479646bbc017caeea147df12fcf`
- `crates/thegn-svc/src/issue/kaneo.rs`: `e444e2f124b7bced1e4d68fd01829062363341b601dfb05640a9cc95ef482ca3`
- `/tmp/thegn-maintenance08-clippy-20260915.log`: `7ae67373510909388456236b6a6e646c0828104431ed4f50626458adae65f573`
