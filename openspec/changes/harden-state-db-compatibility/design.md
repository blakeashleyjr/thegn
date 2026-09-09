# Design

## One database-scoped authority decision

Schema authority is a property of the canonical DB path and the installed
runtime actor, not of whether a caller happened to initialize process-global
state. `Db::open` and any `open_at` path that resolves to the canonical shared
database therefore take the same guarded path. A caller cannot obtain the test
exemption with a lexical alias or symlink. An explicit path is unrestricted
only when it is proven distinct from the canonical database, so hermetic tests
and temporary stores can initialize themselves.

Bootstrap is a separate state, not an implicit policy bypass. An empty
`user_version = 0` database with no prior application schema may initialize
without a production controller unless migration authority is explicitly
disabled. Once canonical application state exists, an absent runtime decision
is a typed fail-closed refusal before DDL or `user_version` changes.

## Operation capability, separate from migration capability

Each DB-consuming command or service entry point declares a minimum readable
and writable schema requirement before opening the shared store. The access
decision has three independent outputs: may open, may use the requested
operation, and may migrate. A compatibility handle joins the schema lease,
rechecks the observed version, and never runs initialization, migrations,
startup pruning, or a `user_version` write. A client may therefore use an older
compatible schema without being allowed to advance it.

An undeclared operation is rejected in tests and cannot receive a compatibility
handle. Missing tables, columns, guards, or lifecycle writes become typed
required-versus-observed capability errors before SQL runs. Callers may not
convert an unavailable guard or lifecycle write into unqualified success;
degraded execution must be part of the operation's explicit contract and
result. In particular, `land` must either execute its remote-target guard and
lifecycle filing against a declared compatible schema or name each omitted
behavior in its result.

## Persistent UI truth

Hydration carries typed schema failures to loop-owned state. A refusal keeps the
last successfully hydrated model, disables mutations whose requirements are not
met, and renders a persistent banner naming the on-disk/build versions and the
rebuild/reinstall plus controller-restart action required. A startup refusal
with no prior model renders an explicitly unavailable state, never an apparently
empty successful model. Generic database unavailability remains a distinct
error kind and is not mislabeled as schema mismatch. The banner clears only
after a successful compatible open and hydration, not on a timer.

## Delivery order

THE-95's fail-closed invariant and test land first. THE-96's requirements model
then gives commands a supported compatibility path. THE-94 consumes the typed
decision for persistent UI feedback. UI work may be developed in parallel but
must test against the final error vocabulary.
