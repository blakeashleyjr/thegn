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

Every shared-store open now chooses one of two explicit contracts. `Db::open`
requests the build's complete current schema and follows migration authority;
an intentionally older-compatible path must instead name a variant from the
closed `SchemaOperation` catalog. Each catalog variant declares separate
minimum readable and writable schema floors plus the exact tables and named
columns it uses. The access decision therefore has three independent outputs:
may open, may use the requested operation, and may migrate. A compatibility
handle joins the schema lease, rechecks the observed version, and never creates
or initializes a database, runs migrations or startup pruning, changes journal
pragmas, or writes `user_version`. A client may therefore use an older
compatible schema without being allowed to advance it.

An undeclared operation cannot receive a compatibility handle because there is
no string or caller-supplied-version API; catalog completeness and non-empty
feature declarations are tested. Missing tables or columns become typed
required-versus-observed capability errors before operation SQL runs. Callers
may not convert an unavailable guard or lifecycle write into unqualified
success. `land` uses a v66 read-only guard declaration when folder lifecycle is
inert and a v66 read/write lifecycle declaration otherwise, holding the shared
lease across guard, fold, and bookkeeping. Preflight errors refuse before git
changes. A later I/O error after the git CAS is reported as an explicit degraded
result (rather than a retry-encouraging ambiguous failure) naming the omitted
sidebar bookkeeping.

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
