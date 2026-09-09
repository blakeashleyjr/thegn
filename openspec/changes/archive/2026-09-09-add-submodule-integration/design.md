# Design — submodule integration

## Core model and service adapter

Core owns pure `.gitmodules` parsing, path safety, gitlink recognition, pointer
direction, summaries, conflict representation, and atomic-selection rules.
The service owns the CLI-backed adapter: it conditionally reads `git submodule
status`, porcelain status, recorded gitlinks, raw pointer diffs, and locally
available commit summaries. This is a provider adapter, not a new defaulted
method on the broad Git backend trait.

All enrichment is best effort and bounded. The read path skips submodule work
when metadata is absent and never fetches commits for UI decoration.

## Atomic editing and views

Mode `160000` and `Subproject commit` lines classify a change as a submodule.
The patch engine refuses a partial gitlink selection; callers use whole-entry
stage/restore operations. Change and drilled-diff models expose old/new
pointers, forward/rewind/diverged/unknown direction, and a bounded local log
when both objects are already present. Numstat's `-/-` is not presented as a
misleading zero-line edit.

The sidebar indicator is an additional submodule-specific signal controlled by
the shipped UI toggle. It does not redefine or suppress every ordinary dirty
state.

## One lifecycle initializer

`git_worktree::initialize` is the shared post-checkout operation. In `auto` it
first checks strict `.gitmodules` metadata, canonicalizes the repository trust
request including declared URLs/paths, and runs recursive init only after
approval. `off` skips it. Failure does not roll back a successfully-created
worktree/clone; it is surfaced as degraded state.

CLI/UI worktree creation, local workspace creation, and remote/provider paths
call this seam after checkout/clone. Workspace clone intentionally remains an
ordinary clone followed by initialization, preserving one trust and failure
contract instead of embedding repo-controlled URL access inside clone flags.

## Merge behavior

A gitlink conflict is reported as `submodule pointer conflict` with path and
both SHAs. It bypasses line-oriented drivers and rerere automation. Resolution
remains an explicit pointer choice by a human or deliberately prompted agent.
