# As-built design

`SidebarState` owns the persisted flat/grouped mode. Each frame projects that
state into `FrameModel.sidebar_flat`, the same state used to build the rows.
The full sidebar header chip names either `flat` or `grouped`; flat uses the
accent colour and grouped uses a quiet colour.

`sidebar_view::header_chips` is the single source of chip geometry for both
painting and mouse hit testing. Clicking the layout chip invokes the same
toggle path as `g`. Chips degrade by whole-chip budget: grouped mode yields the
layout chip before the sort chip, while flat mode keeps the layout chip until
itself no longer fits. The chip is absent while the filter input is open, while
a filter is set, and in the slim rail.

The existing persistence path stores flat mode as `sidebar_flat = "1"` and
restores it on startup. The frame model projects the restored mode each frame,
so row layout and chip label stay aligned. The chip uses theme roles rather
than fixed colours.
