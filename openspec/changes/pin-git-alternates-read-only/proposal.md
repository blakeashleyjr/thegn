# Pin git alternates read-only and validate the object store at launch

## Why

Under `[sandbox.git]` (commit-only, push-lane, full) the shared
`<common>/objects` stays writable, so a sandbox could create or rewrite
`objects/info/alternates` and point the host repository's object lookups at a
planted directory. A repository that already borrows objects from elsewhere
(alternates, a symlink in `objects/`, or a `commondir` that leaves the common
dir) also shares objects whose provenance is outside the sandbox (THE-2148,
WS11-P3-02).

## What Changes

- `objects/info/alternates` and `objects/info/http-alternates` join the
  read-only pins re-bound after the writable `objects/` (created empty, with
  their parent, when missing). bwrap, OCI and systemd lower them like the other
  pins.
- The launch probe reads the alternates file, looks for a symlink at or
  directly under `objects/`, and checks that `commondir` resolves to the common
  dir the worktree's admin dir lives in. No git subprocess is added.
- `sandbox_floor::git_integrity_gate` turns a failing probe into one message.
  `sealed` and `sealed-tunnel` refuse the launch through the home gate; other
  profiles log a warning and launch.

## Impact

- Code: `thegn-core` `config_sandbox::git_policy`, `sandbox`,
  `sandbox_floor`.
- Docs: `docs/help/sandbox-backends.md` ("Git write policy").
- Residual: `objects/` stays writable until WS11-P3-09, so loose objects can
  still be added.
