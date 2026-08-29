REVISE

revision_chunks:

- .thegn/pipeline/THE-88/architect-review/revision-1.md

review_corrections:

- 8583a266 fix(the-88): correct report queue integration

verification:

- merged main into the branch as 720bb6d4
- mandated thegn-core suite: 519 passed
- mandated thegn-host suite: 120 passed
- focused THE-88 suites: 75 core and 50 host passed
- just quick thegn-core and just quick thegn-host passed

remaining_gap: dispatch wait and wake-time DB reads must distinguish a reaped
row from control/transport/database failures; see revision-1.md.
