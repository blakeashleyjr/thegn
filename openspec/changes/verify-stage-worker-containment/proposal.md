# Verify and enforce stage-worker commit containment

Linear: THE-97

## Why

THE-91 repaired the production mount/env chain and a real pipeline worker was
manually shown to commit inside bwrap while an outside write was denied. That
proof is not repeatable: `thegn doctor` does not resolve the effective stage
agent and cannot distinguish a worktree write failure, Git-common-dir failure,
stale callback binary, unavailable provider home, unsupported probe, or an
uncontained launch. A future precedence or mount regression can therefore look
like a dead worker—or silently pair an inner full-access harness with no outer
containment.

## What Changes

- Build a redacted probe plan through the same stage-agent, environment,
  provider-home, callback-binary, sandbox-backend, and mount resolution used by
  a real stage worker.
- Add doctor checks with precise healthy, failed, unsafe, and not-proven states;
  host-side facts are never presented as containment proof.
- Fail closed before automated stage-worker launch when an inner full-access
  harness resolves to backend `none`/uncontained.
- Add a disposable Linux/bwrap smoke that exercises the real containment
  composer, commits through a linked-worktree Git layout, and proves a narrow
  outside write is denied.
- Share low-level backend-probe plumbing with THE-90 where useful while keeping
  commit containment and compiler-cache reachability as independent results.

## Non-goals

- Launching a coding model, contacting a forge, or requiring real provider
  credentials.
- Replacing thegn's outer sandbox with the agent harness's own sandbox.
- Weakening the read-only home or shared `.git/config` protections.
- Treating mount-structure unit tests, host writeability, or backend presence as
  end-to-end containment proof.
- Probing a user's real repository, broad home directory, or unrelated path.
