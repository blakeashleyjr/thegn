# Add a completion value source

Bind a CLI argument to the values it can take, so `<TAB>` on it lists them.
The slot catalog is the only place a binding lives — nothing is attached on the
clap derive.

1. **Declare the kind.** Add a variant to `SourceKind`
   (`crates/thegn-core/src/completion/catalog.rs`) with a doc comment, plus its
   entries in `SourceKind::ALL`, `kind()` (the stable string id),
   `is_implemented()`, and `reads_db()` / `reads_config()`. A value you can
   describe but will not serve is `Reserved(...)` with the reason recorded —
   never a silently missing variant.
2. **Serve it** in `crates/thegn-core/src/completion/sources.rs`, in the family
   the kind belongs to: `DbSource` (read-only state DB), `ConfigSource` (a pure
   function over an already-loaded `Config`), or `StaticSource` (constants
   already in the binary). Every failure path returns an **empty vector**, never
   an error.
3. **Bind the slot.** Add a `CATALOG` row —
   `slot(command_path, arg_id, SourceKind::Yours)` — where the command path is
   space-joined from the root
   (`"wt rm"`, `""` for a root-level argument) and `arg id` is the clap id (the
   field name, or the explicit `id`/`long`). The host walks the catalog and
   decorates the built tree; there is nothing to add in `main.rs`.
4. **Unpin it.** Delete that slot's line from
   `test/completion-slot-ratchet.txt` if it had one. The file is shrink-only,
   and the drift test fails on a stale pin as well as an unbound slot.
5. **Respect the fast-path contract** — this code runs on a keypress. Bounded
   and read-only (no write, no migration, no directory creation); no network, no
   forge call, no subprocess; lazy (a source is constructed only when its slot
   is the one being completed); fail-open (any error completes nothing,
   silently, exit 0); and it checks the `Deadline` if it can do partial work.
   A value that needs git or the network belongs in `Reserved`, not here.
6. **Test it** in the same module — `thegn-core` is 95%-line gated and the
   `completion` module is deliberately not in the justfile's `cov_ignore`.
   Cover the empty case, the error case, and the shape of a candidate's
   description.

**Gates:** `completion_slots_are_bound_or_pinned` (a value-taking argument that
is neither bound, declared `Structural`, nor pinned — regenerate the ratchet
with the `#[ignore]`d `update_completion_slot_ratchet`),
`every_implemented_catalog_slot_actually_binds` (a catalog row whose command
path or arg id no longer exists), the `SourceKind` kind-coverage test,
`just coverage` (95% on the core), `test/smoke.sh` (a live value appears for a
real slot, and a `<TAB>` against an empty state root creates nothing).
