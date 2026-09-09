# Tasks — Unix control peer identity (THE-100)

## 1. Peer metadata

- [x] 1.1 Add platform-native accepted-stream peer credential retrieval for
      supported Unix targets with a testable adapter.
- [x] 1.2 Carry verified/mismatched/unavailable peer context through HTTP,
      WebSocket, and gRPC request authorization.

## 2. Authorization and hardening

- [x] 2.1 Grant implicit local Admin only for peer EUID equal to daemon EUID;
      require scoped token/reject on mismatch or unavailable credentials.
- [x] 2.2 Preserve token-only behavior when `local_admin = false` and provide a
      documented token-required portability escape hatch.
- [x] 2.3 Make run-directory/socket ownership/mode failure surfaced and
      fail-closed whenever implicit local Admin is requested.

## 3. Verification and documentation

- [x] 3.1 Test same UID, mismatched/unknown UID with and without token,
      local-admin off, and hardening failure.
- [x] 3.2 Correct code comments and architecture/config/doctor output so
      same-UID means runtime-enforced identity.

## 4. Reconciliation

- [x] 4.1 Create the active OpenSpec delta linked to THE-100.
- [x] 4.2 Validate the completed change strictly before archive.
