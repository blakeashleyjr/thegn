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
or parser/SQLite message. The additive checked composition layer below validates
final configuration data; capture alone does not confer that property.

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
It does not manufacture a launch callback. Current composition fixtures below
exercise the actual new library seam, not shipping launch-route integration.
Current native gates remain required before this prerequisite can be completed.

## Additive checked composition

HostComposedConfig has a private Config and a read-only accessor. Its Debug is
fixed and redacted. It represents final existing configuration validity, not
permission to launch, raw source provenance of the caller's Config, freshness or
runtime containment. HostCompositionError returns only Source, Bounds or
InvalidFinalConfig, with static diagnostics rather than parser/source values.

capture_and_compose_hosts_checked accepts an already-authorized HostStore and
captures before invoking compose_host_definitions_checked. A malformed shadowed
row therefore refuses before a result can be constructed. The pure composition
operation accepts only a strict HostDefinitionsSnapshot, borrows caller-owned
layered Config, admits its size/work before cloning, invokes the existing THE-598 merge, then
admits and validates the result before returning it. No Db opener, effective
config loader, migration, normalization, secret expansion or provider is called.

The project-specific schema walker remains unchanged, including legacy boolean
failover, union and flattened-map semantics. A shared typed semantic helper
preserves legacy diagnostic order; its checked mode returns after the first
failed validator batch and does not clone subsequent automation profiles.
Individual validator batches retain their existing behavior. Undefined host
references retain existing fallback; disabled model-proxy semantics remain
disabled. No stricter reach requirements or Iroh/Cloud pane support are added.

First cached schema initialization may construct serde defaults, including
environment-derived HOME/USERPROFILE/THEGN_DIR paths for metadata. They are not
installed into the supplied Config. Thus the operation does not claim complete
environment independence or absence of indirect default construction.

## Checked composition work limits

Limits apply only to the additive API, never legacy loaders or config validate.
Before and after merge, streamed serialized JSON is capped at 4 MiB. Structural
admission caps depth at 32, value nodes at 65,536, each map/array at 1,024 entries,
keys at 256 UTF-8 bytes and string leaves at 64 KiB. Pipeline stages and profiles
are each capped at 64; base and explicit profile automation rules at 256.

Before any effective-profile clone, admit at most 4,096 effective rule visits
and 16 MiB of cumulative base/overlay serialized work. Each nonempty profile
charges the original base clone even when its rules replace that base. A
replacing rules list determines effective visits; absent rules inherit base
visits. Counting serialization does not allocate an effective configuration.

These bounds address cubic pipeline traversal and repeated profile/diagnostic
amplification. They are not a total allocator/RSS cap, hard wall-clock deadline,
or bounds on schema-default allocations. Typed trees, bounded JSON, schema and
transient per-validator error vectors coexist. No diagnostic vector is returned
by the new API.

Before recursive serialization or Config cloning, borrowed preflight covers both
dynamic JSON families: model_proxy.providers[].defaults values and flattened
plugins[].contributions[].caps, at actual root depth 5 with one shared budget.
Null caps also consume preflight work. Rejected recursive input stays owned by
the caller; the API never drops it on an error. The serde-skipped sandbox.build
strings/map are independently checked and charged against the same combined
4 MiB budget and node allowance. Build data and issues.accounts_restricted are
preserved in the output. Exhaustive SandboxBuild destructuring forces new build
fields to receive an explicit inventory review; future recursive Config fields
also require updating the documented preflight inventory.

## Current regression boundary

New fixtures use actual private SQLite capture, the production checked wrapper,
shared validation and actual resolve_environment with an explicit local GitLoc,
an owned empty repository/worktree path and denied approvals. They cover source
refusal, unshadowed contribution, declarative Local/SSH winners, full explicit-env
preservation, semantic/schema refusal, redaction and captured-revision behavior.
No provider callback stands in for an actual launch path.

Inclusive boundary fixtures cover exact serialized, structural, stage, profile,
rule and effective-work limits. Test-only scoped observation at semantic entry
and immediately before profile clone proves over-limit input never enters those
operations and checked failure skips later profiles. Legacy ordered diagnostics
and current compatibility branches remain separate positive controls. Native
compilation/execution and final independent source review are pending.

A narrow prerequisite corrects ApiVersion's generated schema to match its
existing string wire format, using String's schema with the stable ApiVersion
name. Serialization, numeric parsing and negotiation remain unchanged. Both
plugin caps and provider-default JSON now have full successful composition
controls at depth 32, with depth 33 refused before cloning. Actual Config
validation covers valid plugins and malformed version/shape controls. This
schema-only correction is tracked under THE-199; its broader contract work
remains open. Native verification of this prerequisite remains pending.
