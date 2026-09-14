# Harden live developer upgrades

THE-613, a bounded child of THE-436, replaces the unsafe `just live` broad daemon
kill and migration-executable override with a staged, confirmed local upgrade.
The launcher must preserve recovery material, respect migration policy, isolate
Cargo artifacts (THE-609), and refuse ambiguous legacy process ownership.

Automatic process shutdown, other developer launchers, named-profile support,
database downgrade and merging queued work are outside this change. Native
readiness and lifecycle ownership protocols remain separate work.
