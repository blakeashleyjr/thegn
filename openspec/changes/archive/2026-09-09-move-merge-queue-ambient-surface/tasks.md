# Tasks — merge-queue ambient surface relocation (THE-9)

- [x] Add a pure per-repo blocked/working/populated rollup shared by the sidebar
      token, rail tone, and optional bars widget.
- [x] Compose the bounded token in the project header's painted geometry and
      expose only its actual cell span for activation.
- [x] Route mouse/keyboard activation to repo-scoped merge-queue detail and keep
      existing row behavior elsewhere.
- [x] Add rail urgency without introducing geometry.
- [x] Remove the default merge-queue badge and provide an opt-in placeable `mq`
      bars widget with default/explicit configuration tests.
- [x] Update configuration, sidebar/bars/merge-queue help, and changelog.
- [x] Cover rollup, attribution, fit/shedding, hit geometry, activation, rail,
      and default/opt-in behavior; land the reviewed THE-9 series through
      `c9f1868c`.
- [x] Strictly validate this change.

## Validation boundary

No historical visual-baseline re-record or separate full-CI invocation is
claimed; unit/render/help and review evidence covers the accepted behavior.
