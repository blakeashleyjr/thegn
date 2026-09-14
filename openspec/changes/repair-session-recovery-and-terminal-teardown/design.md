# Design

Daemon absence queries use the client selected for the preceding failed attach.
They do not discover or start a replacement daemon solely to prove absence.
Sprites uses its documented authenticated GET `/v1/sprites/{name}/exec` roster
with strict array/identifier decoding; only a successful complete roster can
prove absence. Named sessions stay on their native provider transport; iroh
streams currently have no persistent session identifier.

The relay retries the same identifier, with an attempt timeout and overall
budget, while keeping controls in its existing bounded receiver. Recovery must
notice explicit closure even when buffered input remains; any retained pending
control has a bounded owner and is delivered only to the recovered session.
Unknown outcomes never authorize a second fresh open. Flapping successfully
attached streams retain their existing no-progress budget.

Final launch admission uses the existing honest capability classification. It
runs before returning host/bare outcomes as well as before sandbox creation.
Floor-off and provider-managed bypass behavior remain unchanged. Runtime probe
warnings distinguish executable presence from service health and permissions.

Termwiz is vendored from the exact cached 0.23.3 release with its license and
provenance. Only destructor error handling and regression tests change upstream
source. Cleanup attempts all mode restoration and signal unregistration even
after write failure. Production does not catch and suppress destructor panics
or leak terminal ownership. Linux PTY tests are runtime evidence; Windows
changes are best-effort destructor symmetry and require Windows runtime CI.

All recovery work remains on the relay task, not the UI loop. Existing output,
fallback and exit events pulse the terminal waker and select the existing pane
damage channel. No SQLite schema/version or help-context change is required.

Provider protocol source: https://sprites.dev/api/sprites/exec (official API,
reviewed September 13, 2026). Non-200 and malformed responses remain errors;
they are not an empty roster.
