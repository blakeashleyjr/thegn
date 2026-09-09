# Tasks — pipeline board

- [x] Drain bounded adoption intents through the existing daemon-backed pane
      attachment path (`7e65ffd8`).
- [x] Replace the proposed monitor tab with the accepted standalone Pipeline
      Board overlay and `open-pipeline-board` action (`42984dc4`).
- [x] Deliver stage-column and narrow stacked layouts, empty configured stages,
      stable dispatch-ID selection, status/stalled facts, and board help/footer
      behavior (`42984dc4`, `9bc47777`).
- [x] Route row activation through shared resident and dormant worktree
      activation paths (`42984dc4`, `c037cbab`).
- [x] Keep roster I/O off-loop, gate periodic sampling on board visibility, and
      align damage with closed-sidebar versus open-overlay consumers
      (`42984dc4`, `cb34db84`).
- [x] Normalize legacy second-based dispatch timestamps to milliseconds before
      age and stalled calculations (`42984dc4`).
- [x] Publish live-stage evidence consumed by the sidebar while leaving exact
      project nesting to `nest-sidebar-pipelines` (`7e65ffd8`, `42984dc4`).
- [x] Cover adoption, layout/reflow, stable selection, activation, hydration,
      timestamp, and render-plan behavior with focused unit/integration tests
      recorded in the delivery and review commits.
- [x] Reconcile the OpenSpec contract to the accepted THE-74 implementation and
      strict-validate the change before archive.

## Validation boundary

The recorded delivery and review evidence is accepted for this documentation
reconciliation. No new live GUI run or repository-wide CI result is asserted.
