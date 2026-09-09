# Add the pipeline board

> Reconciled to the accepted implementation completed under THE-74. The
> original system-monitor-tab design was superseded during implementation by a
> standalone board overlay.

## Why

The durable dispatch roster and daemon-session adoption path existed, but a
human supervising a staged pipeline still needed one place to see active work,
understand its stage, and jump into the relevant worktree. Legacy roster
timestamps also mixed seconds and milliseconds, which made age-based UI facts
unreliable.

## What Changed

- The compositor drains bounded `adopt_session` intents through the existing
  daemon attach path, so externally launched stage agents can become ordinary
  resident panes without stealing focus.
- A standalone Pipeline Board overlay presents the roster by configured stage.
  Wide layouts use stage columns; narrow layouts stack stages. Stable dispatch
  IDs preserve selection across refreshes.
- Rows expose status, stalled state, agent, concurrency, issue/artifact, age,
  and next-stage facts without advancing the pipeline or enforcing policy.
- Activating a row uses the shared worktree activation path, including the
  dormant-worktree fallback when no current sidebar row exists.
- Roster reads stay off the event loop. The existing refresh/waker path carries
  samples, dirty work is coalesced, and periodic sampling is gated by board
  visibility.
- Closed-board roster changes can update derived sidebar stage evidence with a
  bounded sidebar diff; an open board follows the normal boxed-overlay rule and
  requests a full frame.
- Legacy second-based dispatch timestamps are normalized to milliseconds at
  the persistence/read boundary.

## Accepted Scope

The board is read-only over dispatch state. It does not move work between
stages, enforce concurrency, invent a new scheduler, or add a new wake source.
Exact project-nested sidebar placement is owned by
`nest-sidebar-pipelines`; this change supplies the stage evidence consumed by
that surface.

## Evidence

- Original adoption and roster substrate: `7e65ffd8`.
- Accepted Pipeline Board v2 and timestamp normalization: `42984dc4`.
- Review follow-ups: `9bc47777`, `c037cbab`, `cb34db84`.
- Runtime seams: `crates/thegn-host/src/pipeline_board/`,
  `crates/thegn-host/src/run.rs`, and
  `crates/thegn-host/src/render_plan.rs`.

## Impact

- Extends the `agent` capability with a visible, navigable dispatch surface.
- Reuses the existing daemon, refresh, action, and worktree-activation seams.
- Does not add a separate monitor tab, database polling thread, or stage
  transition authority.
