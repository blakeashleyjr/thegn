# Effective host selection

`merge_host_defs` obtains `cfg.host.entry(name).or_insert_with(...)` once. The
resulting entry governs both reach-to-placement selection and copied SSH
settings. An existing `cfg.env[name]` remains untouched. SSH/local winners keep
their existing pane support; Iroh/cloud winners still do not synthesize pane
environments. This preserves existing host-binding behavior during actual
`resolve_environment`, including explicit environment host pins.

The change touches no render damage channel, event-loop wake path, SQLite
schema/version, config table, help context or interactive action. It neither
reads the DB nor repeats definition capture; callers still supply a slice.

Five regression tests cover 19 precedence cases through the actual merge and
environment resolver. Private empty directories, root equal to worktree, and
explicit local GitLoc avoid DB discovery and the main-branch Git probe. Only
private absent repo-overlay reads occur. Exact SSH destination, port, transport,
forwarding, config, jump host, identity and arguments are checked, as are full
host/env preservation and unresolved non-pane selections. No env mutation,
transport execution or live routing reproduction is required.

Native tests and an old-body counterfactual are pending. Restoring only the old
merge body while retaining the tests must fail the declared-local/DB-SSH case.
Source review and formatting alone do not establish native acceptance.
