# Panel

## ADDED Requirements

### Requirement: A structural diff view is available on read-only diff surfaces

When `[git] structural_diff` selects a structural differ (difftastic), the
full-screen diff view SHALL render the diff through the external tool's
ANSI output, converted by an in-process SGR-subset parser (unknown escape
sequences stripped, colors composed in truecolor and quantized at the
existing chokepoint), sized to the view's content width, and togglable back
to the internal unified view with a bound key. The structural path MUST be
read-only and best-effort: the tool runs off the event loop under byte/graph
limits and a timeout, and any failure (tool missing, oversize input, parse
error, non-zero exit) falls back to the internal unified view with a
one-line notice. Stageable diffs (inline hunk previews, staging, `git
apply` inputs) MUST continue to pin the sanitized internal diff
(`--no-ext-diff`) regardless of this setting. With `structural_diff =
"auto"` the structural view applies only when the tool resolves through the
managed-tool tiers.

#### Scenario: Structural rendering in the diff view

- **WHEN** `structural_diff = "difft"`, the tool resolves, and the user opens
  the full-screen diff view
- **THEN** the diff renders from difftastic's output at the view's width, and
  the toggle key switches to the internal unified view and back

#### Scenario: Failure falls back, never blocks

- **WHEN** the structural tool is missing, times out, or exceeds its size
  limits
- **THEN** the internal unified view renders with a one-line notice, and the
  event loop was never blocked by the attempt

#### Scenario: Staging is never structural

- **WHEN** any staging surface produces a diff while `structural_diff` is
  active
- **THEN** the diff is generated with the sanitized internal flags and
  round-trips through `git apply` exactly as before
