# Design

The worker scores a snapshot, never live buffers. A pane whose write count and
line count are unchanged is reused as the same `Arc`; a changed pane is cloned
at one refcount bump per line. The global rank needs the whole scan, so the
worker finishes, sorts and caps, then streams the ranked list in batches; the
batches bound channel and redraw cost, they do not show provisional ranks.

A jump or preview resolves against the snapshot the results came from. The jump
target is located in the live grid by line content with the snapshot distance
as a hint, so lines pushed since the snapshot do not mis-aim it.
