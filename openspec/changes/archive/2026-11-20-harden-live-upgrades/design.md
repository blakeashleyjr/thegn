# Design

A Python standard-library helper owns the developer upgrade transaction; `just`
passes positional arguments without shell interpolation. It stages release
profiling artifacts independently of ambient shared Cargo output directories,
checks the narrowly supported migration settings without invoking a stateful
application command, and requires interactive installation confirmation.

Legacy controller shutdown is explicitly manual. Same-user process inspection
and the native SQLite schema lock provide conservative checks, not proof against
noncooperating concurrent launches. State-root and installation-directory locks
serialize cooperating helper instances, including different state roots using
the same installed binary. Paths and ownership must be unambiguous.

Before atomic replacement, a bounded online SQLite backup and independent old
binary copy are retained privately. Neither an interrupted launch nor a newer
schema authorizes automatic restoration. The normal controller remains the
migration authority; the helper never overrides the executable pin. Launch
status is not application readiness. Brand-directory migration is disabled for
this bounded path layout, separately from database migration.

Tests use private temporary files, databases and controlled subprocess fixtures.
No test stops or replaces the user's running application.
