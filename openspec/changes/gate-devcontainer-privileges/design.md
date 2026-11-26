# Design: gate devcontainer privilege requests

## Decision

Represent privilege requests and their origins with shared enums in
`thegn-core`. Expose the engine socket paths from the runtime-escape module and
reuse them in the Compose guard, preserving its path and class order. Treat a
bind source as protected when its canonical path equals or contains an engine
socket or thegn control/state directory. Resolve symlinks and normalize
nonexistent paths after variable substitution.

Protected bind mounts are omitted from both the sandbox mount list and the
generic `devcontainer.mounts` request. Report `refused: engine-socket` even
before an ordinary mount approval is available. There is no grant path in this
change, so the refusal is unconditional.

## Security boundary

Path checks cover direct binds, ancestor directories, and symlinks. They do not
cover host bind mounts layered later to expose a protected socket; Compose and
the post-start checks provide separate coverage.
