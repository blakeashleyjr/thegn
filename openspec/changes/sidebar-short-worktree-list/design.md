# Design

Protected rows (active, cursor, pinned, blocked, failed) count toward the
limit but are never folded; a batch row holding one is protected. Batch rows are
derived (`RowKind::BatchGroup`), emitted only in the grouped layout, keyed
`batch:{slug}/{token}`, and persist their open state as `batch_open:<key>`.
The detail line and the heights pass share one `TitleEcho`. Counters stay on the
title line by an earlier decision.
