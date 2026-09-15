# Source validity without launch authority

The new HostStore::host_defs_checked returns immutable HostDefinitionsSnapshot
data or a typed, static diagnostic. It never calls the legacy host_defs path,
opens a database, installs policy, resolves secrets or provisions a host. Legacy
readers keep their existing display compatibility. There is no snapshot
constructor from arbitrary deserialized Config, no Deserialize/Default and no
freshness/ownership/runtime-proof claim.

## One read transaction

Require an already-authorized connection in autocommit mode. Begin a deferred
read transaction and read main.user_version fallibly inside it. This initial
contract requires exactly this build's SCHEMA_VERSION; unsupported versions are
errors, not default schema0 or empty hosts. The same transaction checks ordinary
main.hosts metadata and captures rows; no version/data gap across a concurrent
writer. Temporary hosts objects cannot shadow main-qualified reads.

Use pragma_table_list type metadata instead of copying arbitrary sqlite_schema
SQL. Bound borrowed table/column names before owning them; require ordinary
nonhidden columns, host_id TEXT primary key, name TEXT and config_json TEXT.
Other ordinary columns are permitted within the128-column bound. No migration,
query_only/journal/busy-handler changes, pragma writes or hidden nested commit.

## Bounds and raw decoding

Fixed limits are1024 definitions,8192 total inventory rows,128 schema columns,
256-byte names,4MiB per JSON and4MiB aggregate copied names/JSON. Stream all rows
with one extra limit sentinel, no SQL WHERE or ORDER BY: sparse NULL definitions
cannot hide unbounded row scanning or sorting. NULL config_json remains an
inventory row without a user definition; malformed non-NULL values are errors.
SQL returns type/byte probes and CASE-guarded payloads; Rust validates borrowed
types/UTF-8/lengths/aggregate before String copies. Duplicate/empty/control names
are rejected and successful output is deterministically name-sorted in Rust.

After committing the read-only capture, decode exact captured JSON bytes with
duplicate-key detection at every object, depth32/node16384/key256-byte bounds,
then the existing HostConfig schema walker, then permissive typed decode. This
ordering refuses unknown fields and bad enum values before they can disappear.
Valid existing enum aliases remain accepted. Errors contain no source payload
or parser/SQLite message. Final composed semantics and host/env precedence remain
the later checked configuration layer's responsibility.

The generated HostConfig schema is cached once per process, not rebuilt for
each row. An additive root-taking validator helper shares the exact existing
walker; the generic validator and its public behavior remain unchanged. The
cache contains only schema, never caller config or authority state.

## Explicit limitations

The row/byte limits bound application copies and ordinary cursor work, not all
SQLite internal page memory or arbitrary VFS/query execution time. A configured
busy timeout bounds lock waits, not query lifetime. No progress hooks/dependencies
or connection-global mutation are introduced. Host workers still own bounded
concurrency, response deadlines and truthful cancellation/lifetime behavior.

This operation requires an already-authorized connection. It does not fix the
read-only opener's is_file/version-error conflation, WAL path binding or global
startup ordering (THE-603/THE-592). It must not be exposed as completed launch
admission. Tests are private SQLite fixtures; no live state reads or mutations.

## Recovery and acceptance status

This component is recovered from retained9d744a12 against current main. Its
historical50-test summary has no recovered raw receipt and is not a current
passing gate. The new malformed-shadowed regression calls the actual object-safe
HostStore capture seam with a real private SQLite row; a valid same-name persisted
SSH definition is its positive control, then each malformed replacement must
refuse while the declarative Local config and stored bytes remain unchanged.
It does not manufacture a launch callback or checked final-config adapter.
Final semantic composition after host augmentation remains missing acceptance
for THE-602, independent of the separately owned THE-598 winner repair. The
storage prerequisite can land partially without marking the full issue Done.
