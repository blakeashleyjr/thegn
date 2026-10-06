# THE-410 plan

Scope: gtui-core frame accessors borrow; renderers (logs/table/timeseries/stat) bounded by panel rect; bounds_for no clone.
Approach: floats()/strings() return slices; cell_str Cow; logs/table take rows+width and build only visible rows, clip cells; timeseries downsamples min/max per pixel bucket only above 4 samples/pixel (identical below).
Tests: buffer equivalence vs old implementation; bounded downsample keeps extrema and gaps.
Out of scope: host-side frame-generation cache, benches, key-latency integration test (separate seams).
