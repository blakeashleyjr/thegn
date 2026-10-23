# Persistent sidebar layout feedback

Linear: THE-93

## Why

Bare `g` changes and persists flat/grouped mode, but the sidebar header exposed
sort state only. After the transient message disappeared, an intentionally flat
tree was indistinguishable from missing grouping data.

## What Changes

The full sidebar header names the current layout with a lowercase `flat` or
`grouped` chip. Flat uses the accent colour; grouped uses a quiet colour. The
chip derives from the persisted row-layout state each frame, shares geometry
between painting and hit testing, and clicking it toggles the mode through the
same path as `g`.

The header drops chips whole as width tightens: grouped mode drops the layout
chip before sort, while flat mode keeps it until it no longer fits. Filtering
and the slim rail hide the layout chip.

## Scope boundary

The delivered change adds persistent visual feedback for the existing layout
mode. It does not change how rows are grouped or the meaning of `g`.
