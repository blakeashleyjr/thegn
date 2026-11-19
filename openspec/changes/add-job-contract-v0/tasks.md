## 1. Shared contract foundations

- [x] 1.1 Implement strict YAML inputs, terminal result types, structural JSON Schema export and the portable feature set. Exercise defaults, rejected combinations and the RFC's sharded example.
- [x] 1.2 Pin canonical REAPI directory bytes, digest and executable-bit behavior with a golden fixture.
- [x] 1.3 Implement pure lease transitions and a portable expiry, reassignment and commit-replay vector.
- [x] 1.4 Specify the v0 guest Hello envelope, bounded reader/writer and protocol/spec negotiation with a golden frame.

## 2. Verify the contract at its consumers

- [ ] 2.1 Replay the lease vector through durable SQLite transactions including concurrent claims, cancel versus commit, restart, artifact visibility and idempotent accounting.
- [ ] 2.2 Exercise a captured dirty worktree with the local executor, verified launch admission and immutable input root.
- [ ] 2.3 Extend the guest schema and vectors to assignment, step execution, sequenced output, cache/file transfers and result publication. Qualify the local guest with the black-box suite.
