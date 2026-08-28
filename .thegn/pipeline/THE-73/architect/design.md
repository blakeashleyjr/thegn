# THE-73 — Sidebar drops git-listed worktrees outside `worktrees_dir` on click-resync

Architect design. Branch `tg/the-73-sidebar-reap`.
Linear: https://linear.app/blakeashley/issue/THE-73

---

## 0. Correction to the issue's stated mechanism (read this first)

The issue says _"the click-path resync reconciles away rows whose paths are
outside `worktrees_dir`"_. **That predicate does not exist on any UI path.** I
grepped every `worktrees_dir` reference in the workspace:

```
crates/thegn-core/src/worktree.rs:199      worktree_path()      — where a NEW worktree is placed
crates/thegn-host/src/onboarding.rs:222…   — the setup wizard's editable field
crates/thegn-host/src/cmd/doctor.rs:629    — a doctor path probe
crates/thegn-host/src/cmd/doctor.rs:1674   — a doctor path probe
crates/thegn-host/src/run.rs:932           — which filesystem the `disk` widget measures
crates/thegn-host/src/cmd/list.rs:13       — `is_managed()`, the `thegn list` CLI filter
```

The only containment test is `cmd/list.rs:12-16` (`is_managed`), and it is
reachable **only** from `thegn list` (`cmd/wt.rs:134`); nothing in the
compositor, the sidebar, the hydration thread or the session resurrect calls it.
So there is no `worktrees_dir` reap to delete.

The observed behaviour is real, and it _is_ the local-foreign-dir sibling of the
`row_is_remote` reap — but the key that loses the row is **the recorded
`repo_path` / dir-existence bookkeeping**, not directory containment. The fix
shape the issue prescribes ("key on git worktree membership / repo root, not on
a path predicate; add a guard test mirroring `row_is_remote`'s") is exactly
right; this design applies it to the three chokepoints that actually decide the
row's fate.

---

## 1. How a worktree row reaches the screen

Two mutually exclusive sources, chosen per workspace by one line.

`crates/thegn-host/src/sidebar.rs:1173-1258` — `gather_groups`:

```rust
for (gi, g) in session.worktrees.iter().enumerate() { …push live Group… }   // 1182-1202
let live = !groups.is_empty();                                             // 1203
if !live && !repo_path.is_empty() {                                        // 1210
    …synthesize Groups from db_by_slug (the registry rows)…                // 1211-1255
}
```

So for one workspace:

- **live** (the session holds ≥ 1 group with that `{slug}/…` prefix) → the
  sidebar renders **only `session.worktrees`**. Every registry row is used for
  decoration only (`db_by_tab` lookups at 1189-1198) and contributes **no row**.
- **dormant** (no session group) → the sidebar renders **only the registry
  rows**.

Both the grouped emitter (`sidebar.rs:877`) and the flat emitter
(`sidebar.rs:1355`) go through this one function, so the asymmetry is global.

**This is the doctrine violation.** A registered, git-listed worktree of a live
workspace is invisible unless the _session_ happens to carry it, and the session
carries exactly what `Session::resurrect_with_cfg` adopted. That makes an
adoption miss present as data loss.

### Why that produces the exact reported repro

1. The workspace is dormant → the registry rows render → the foreign-dir
   worktree **shows** ("full rehydrate / workspace switch shows them").
2. The user clicks that row. Its target is
   `RowTarget::Workspace { repo_path, group }` (`sidebar.rs:1230-1233`,
   `1249-1252`) → `handlers/sidebar_activate.rs:92-118` →
   `run.rs::switch_workspace:1966` → cold arm `run.rs:2042` →
   `session.rs::switch_to_workspace_deferred:712` → `Session::resurrect(db, repo_path)`
   at `session.rs:727`.
3. The workspace is now **live**, so `gather_groups` flips to the session-only
   branch. Any registry row `resurrect` did not adopt **vanishes**.
4. Switch away → dormant again → registry rows render → the row **returns**, DB
   untouched ("data intact throughout").

That is the whole symptom, including the "click makes it disappear, switching
brings it back" signature, with no reap and no deletion involved.

### The adoption predicate that drops it

`crates/thegn-host/src/session.rs:376-409` (inside `resurrect_with_cfg`):

```rust
if !crate::hydrate::row_is_remote_effective(…) && !Path::new(&wt.worktree).is_dir() {
    continue;                                                            // 382-392
}
let known = |ws: &[WorktreeGroup]| ws.iter().any(|g| g.name == wt.tab_name);
let adopt = (wt.session_name == session && !known(&worktrees))            // 398
    || (wt.repo_root == session                                           // 399
        && wt.tab_name.starts_with(&format!("{slug}/"))                   // 400
        && !known(&worktrees));                                           // 401
```

Three independent ways a genuine, git-listed worktree fails this:

- **(a) `wt.repo_root == session` is a byte-compare of two path strings.** The
  registry row's `repo_path` is whatever the _registering_ process resolved
  (`cmd/wt.rs:274` `root.to_string_lossy()`), and `session` is whatever the
  workspace pointer holds (`hydrate.rs:996-1014`). A CLI running under a
  different `$HOME`, through a symlinked checkout, or with a differently
  normalised root produces a different string for the same repo → arm 2 fails.
  Arm 1 fails too, because `put_worktree` stamps `session_name = session()`
  (`db_workspace.rs:267`), which is the literal `"default"` for every row this
  machine has (verified against the live DB: all 16 rows carry
  `session_name = "default"`), never the workspace path.
  The redundancy is the bug: the `{slug}/…` prefix on line 400 **already**
  identifies the workspace uniquely — `slug` comes from
  `repo::repo_slug_with(db, session_path)` (`session.rs:361`), and
  `db.slug_for_repo` assigns one globally-unique slug per repo-root string
  (`repo.rs:76-104`, table `repo_slugs`). Requiring the `repo_path` string to
  byte-match **as well** can only ever lose rows that the slug already proved
  belong here.

- **(b) `!Path::new(&wt.worktree).is_dir()` (line 389).** A transiently
  unreadable/unmounted dir (sshfs projection, a slow autofs mount, a
  `.claude-profiles/<p>/…` tree whose parent is momentarily gone) is treated as
  proof of deletion. `row_is_remote_effective` is the only exemption, and it only
  covers off-host placements.

- **(c) `switch_to_workspace_deferred` uses the default-config shim.**
  `session.rs:727` calls `Session::resurrect(db, repo_path)`, whose own doc
  (`session.rs:314-321`) says the shim exists for "callers without a loaded
  config (tests, **workspace switch**)" and falls back to
  `Config::default()`. With an empty `cfg.env`, `row_is_remote_effective`
  cannot see a non-local placement, so on the _workspace-switch_ path — the
  exact path the repro clicks — even a genuinely remote worktree is classified
  local and dropped by (b). That is the `row_is_remote` bug re-opened on one
  caller, and it is in scope for the same reason.

### The two hard reaps (data-destroying, same key)

Both key on `is_dir` alone, exempting only remote rows, and both **DELETE the
registry row**:

- `crates/thegn-host/src/hydrate.rs:1388-1408` — `db_worktree_list`, on the
  hydration thread: `db.del_worktree(&w.worktree)` +
  `thegn_core::activity::forget`.
- `crates/thegn-host/src/hydrate.rs:916-933` — `prune_stale_worktree_groups`,
  from `load_or_seed_session` (`hydrate.rs:1061`): partitions the session on
  `g.path.is_empty() || remote.contains(&g.path) || Path::new(&g.path).is_dir()`
  (line 922) and `del_worktree`s the losers.

Neither asks git. "The directory is not there **right now**" is not the same
claim as "git no longer lists this worktree", and only the second one licenses
destroying the row. This is where the issue's prescribed guard belongs.

---

## 2. The fix

Three independent, file-disjoint changes. Each is a strict _widening_ — nothing
that renders/adopts today stops rendering/adopting.

### F1 — the sidebar renders the union, not one source or the other

`sidebar.rs::gather_groups`: keep the live loop as-is, then **append** the
registry rows for this slug that no live group already covers (matched by
`tab_name` first, then by `path`, so a group renamed in-session doesn't
duplicate). The appended rows use the same `RowTarget::Workspace { repo_path,
group: Some(tab_name) }` shape the dormant branch already emits, so activation
behaviour is unchanged from what a dormant row does today.

Drop the `!live` gate; keep the `!repo_path.is_empty()` gate (a live-fallback
workspace entry carries an empty `repo_path` — `hydrate.rs:1285-1293` — and has
no switch target). The synthetic `home` row is still emitted only when no live
group covers `{slug}/home`.

This makes the reported symptom **structurally impossible**: whether a
registered worktree renders no longer depends on whether the session adopted it,
so a future adoption miss degrades to "row is not focus-clickable in-place"
rather than "row disappeared".

### F2 — `row_is_git_listed`: only git may condemn a row

Mirroring `row_is_remote` / `row_is_remote_effective` (`hydrate.rs:1317-1370`),
add to `hydrate.rs`:

```rust
/// Whether git still lists `worktree` as a worktree of `repo_root`.
/// Consulted ONLY on the reap branch — the happy path spawns nothing.
pub(crate) fn row_is_git_listed(
    repo_root: &str,
    worktree: &str,
    cache: &mut HashMap<String, Vec<String>>,
) -> bool
```

Built on the existing pure parser: `thegn_core::util::git_out(root, &["worktree",
"list", "--porcelain"])` + `thegn_core::util::parse_worktree_branches`
(`util.rs:447-465`). Memoised per `repo_root` for the pass, so at most one
subprocess per repo even with many missing dirs, and **zero** when nothing is
about to be reaped. An unreadable/absent `repo_root` (`git_out` → `None`)
returns `true` — fail-safe: we could not prove deletion, so we do not destroy.

Wire it into both reaps as the _last_ condition, after the cheap `is_dir` and
`row_is_remote_effective` checks:

- `db_worktree_list` (`hydrate.rs:1388-1408`)
- `prune_stale_worktree_groups` (`hydrate.rs:916-933`)

Comparison is on the path string as git prints it (absolute, resolved), against
the recorded `worktree`, both normalised with `Path::new(..)` component
equality — no `worktrees_dir`, no prefix test. A guard test asserts that a row
whose path is far outside `worktrees_dir` survives when git lists it.

**Invariant check.** `db_worktree_list` runs on the hydration thread — fine.
`prune_stale_worktree_groups` runs inside `load_or_seed_session`, i.e. _before
the first frame_, where CLAUDE.md forbids blocking subprocesses. The guard is
therefore strictly reap-branch-only: in the steady state (no missing dirs) it
spawns nothing and launch latency is byte-for-byte unchanged. The only case that
pays is the one that was about to silently delete the user's registry row, and
one `git worktree list` is the correct price for not doing that. State this in
the comment so a future perf sweep doesn't "optimise" it back onto the fast
path.

### F3 — adoption keys on repo identity, not on a repo-path string

`session.rs:398-401`: drop the `wt.repo_root == session` conjunct from arm 2.
The `{slug}/…` prefix already carries the identity (see §1(a)). Arm 1
(`session_name == session`) stays, so nothing that adopts today stops adopting.

`session.rs:727`: thread the config through so the workspace-switch path stops
using the default-config shim — `switch_to_workspace_deferred` gains a `cfg:
&Config` parameter and calls `resurrect_with_cfg`. Callers:
`session.rs:695` (`switch_to_workspace`, which likewise gains the parameter) and
`run.rs:2042`.

Because `run.rs` is ratchet-pinned and belongs to no chunk, F3's chunk owns the
**one-line** call-site edits in `run.rs` and any `switch_to_workspace` test
callers; it must not touch `run.rs` otherwise. (If threading `cfg` into
`switch_to_workspace_deferred` turns out to require more than adding the
parameter and updating call sites, the coder should stop at the `repo_root`
conjunct removal and record the shim as a follow-up in the chunk's report
rather than expanding the diff.)

---

## 3. What is deliberately NOT done

- **No `worktrees_dir` predicate is added or removed.** `cmd/list.rs::is_managed`
  stays as-is: it is the CLI's _inventory_ filter (and already has an
  `|| branch.starts_with(&cfg.branch_prefix)` escape), not a reconcile.
- **No new git call on the render/switch fast path.** F1 is pure; F3 is pure;
  F2's probe is reap-branch-only.
- **No schema change / migration.** All three fixes are read-side.
- **No `thegn-core` change.** The core stays substrate-free and its 95% gate is
  untouched; `git_out` / `parse_worktree_branches` already exist there.

## 4. Invariants and ratchets touched

| Invariant                                       | Status                                                                                                                                                                                                                          |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0% idle / no blocking I/O on the loop           | F2's probe is on the hydration thread + the pre-frame prune's reap branch only; F1/F3 are pure. No new wake source.                                                                                                             |
| Render decision is pure (`render_plan::plan`)   | Untouched — no change to the render path or its work-shape.                                                                                                                                                                     |
| Degrade at the edges (colour/glyph chokepoints) | No draw sites touched.                                                                                                                                                                                                          |
| `thegn-core` substrate-free + 95% lines         | No core change.                                                                                                                                                                                                                 |
| Seams, not vendors                              | The `git` call is `thegn_core::util::git_out`, the existing seam; no new vendor CLI.                                                                                                                                            |
| git is the source of truth for worktrees        | This is the change: F2 makes git the only authority that may condemn a row.                                                                                                                                                     |
| Architecture ratchets (`test/*-ratchet.txt`)    | None triggered: no new `#[cfg]` outside `platform/`, no colour/glyph literal, no `gh` call, no `async fn` in a provider trait, no ignored `Result` on a user-visible path, no new idle poll. **No ratchet entry may be added.** |
| Help ratchet                                    | No new `ACTION_SPECS` action, keybind, zone or panel section → no `docs/help/` change needed.                                                                                                                                   |
| god-files                                       | `run.rs` gains only call-site argument edits; new logic lands in `hydrate.rs`/`sidebar.rs`/`session.rs` next to its siblings.                                                                                                   |

## 5. Test plan (scoped — no full-workspace gates)

Everything lands in the host crate's existing inline/sibling test modules:

- `crates/thegn-host/src/sidebar.rs` `mod tests` (line 1667) — sits beside
  `dormant_workspace_renders_same_structure_as_live` (2962-3100), the natural
  mirror for F1.
- `crates/thegn-host/src/hydrate_tests.rs` — sits beside the `row_is_remote` /
  `row_is_remote_effective` guard tests (822-900), the mirror the issue asks for.
- `crates/thegn-host/src/session.rs` `mod tests` (line 847) — beside
  `resurrect_normalizes_legacy_home_prefix_and_preserves_active` (1392).

Per chunk: `just quick thegn-host` + a filtered
`cargo nextest run -p thegn-host <filter>`. **No `just test`, no `just ci`, no
`just coverage`, no e2e** — those are the Lead's pre-push gates.

## 6. Chunking

| #   | Files                                                                                | Depends on | Parallel-safe |
| --- | ------------------------------------------------------------------------------------ | ---------- | ------------- |
| 1   | `crates/thegn-host/src/sidebar.rs`                                                   | —          | yes           |
| 2   | `crates/thegn-host/src/hydrate.rs`, `crates/thegn-host/src/hydrate_tests.rs`         | —          | yes           |
| 3   | `crates/thegn-host/src/session.rs`, `crates/thegn-host/src/run.rs` (call sites only) | —          | yes           |

File sets are disjoint, so all three can run concurrently. They are also
independently valuable: 1 removes the symptom, 2 removes the data-destroying
reap, 3 removes the adoption miss that started it.

Chunk specs: `.thegn/pipeline/THE-73/code/chunk-{1,2,3}.md`.
