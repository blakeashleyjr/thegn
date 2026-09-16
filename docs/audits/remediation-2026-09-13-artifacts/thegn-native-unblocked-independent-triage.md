# Independent rerun triage

Read-only inspection of the final-candidate rerun artifacts confirms 99/99
previously failed tests now pass: host97/97 and service2/2. Old and new test-name
sets match exactly, with no omissions, additions or duplicate test records.
Every record has exit0/status passed, and every per-test log explicitly reports
1 passed,0 failed,0 ignored. No failure or timeout remains in these selected sets.

Inputs:

- /tmp/thegn-audit-combined-host-results.json
- /tmp/thegn-audit-combined-svc-results.json
- /tmp/thegn-native-unblocked-host-results.json
- /tmp/thegn-native-unblocked-svc-results.json

All64 directly observed ownership refusals,30 inferred history/downstream cases,
four listener EPERM cases and the GPG prerequisite failure now have passing
receipts. This supplies positive execution for the30 previously inferred cases;
the earlier failure report remains an accurate historical record of that run.

The signed-fold fixture reports1pass in0.87s. Source probes gpg availability,
generates a private throwaway key, asserts a landed result and a gpgsig header,
and verifies the resulting commit using its private loopback wrapper. The rerun
runner retains PATH and isolates state/Git configuration; gpg is available on the
reviewer's inherited PATH. Its only absent-binary early return is unchanged.
The successful log itself does not print intermediate GPG command output.

No actual regression or smallest code revision is indicated by these artifacts.
No code edits, Cargo runs, test reruns, permissions changes, process signals or
provider actions were performed by this reviewer. These99 selected results do
not substitute for unrelated remaining native/platform or full-suite gates.
