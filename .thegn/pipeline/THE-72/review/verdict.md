# THE-72 — security/test/bug review verdict

Reviewer lane: security / tests / bugs. Branch `tg/the-72-linear-backend`,
reviewed against `architect/design.md`, `architect-review/verdict.md`
(APPROVED) and both coder chunk docs — their "Unverified" sections and the
architect-review "Follow-ups" / "Accepted deviations" were the checklist.

Verdict: **PASS**

PASS

Pass — ready for the merge queue. One review fix committed (pre-existing main
debt surfaced by the mandated merge); no THE-72 defect found.

## 0. Merge of main (done first, per addenda)

`git merge main` — clean tree merge, one commit `2800ba8b`. Two lane-doc files
came in mid-flip of the known markdown-table treefmt oscillation; both were
formatted to the stable state and are committed with this verdict (formatting
only, no content change). Post-merge `git diff main...HEAD` reviewed in full:
24 files, all of them the THE-72 lane docs plus the code files below. The merge
introduced no conflict resolution of mine beyond staging.

Note: the merge also surfaced a **red `just lint` from main itself** — THE-83's
`crates/thegn-host/src/config_source.rs` landed with two unpinned ignored
Results (see §3, fixed).

## 1. Lead's risk surface, item by item

**Recorded-schema contract test is not a tautology — confirmed.**
`linear_schema.json` + `linear_schema_tests.rs` (12 tests):

- The fixture's `Issue` type records singular `assignee: "User"` and **no**
  `assignees`; `recorded_issue_type_has_no_assignees_field` pins that, so the
  fixture cannot be "fixed" to match a drifted selection.
- The checker validates every field of every selection (`ISSUE_FIELDS`, the
  get/comments selection, the create mutation's shared selection) against the
  fixture and reports _all_ unknown fields, not the first
  (`parse_selection_handles_nesting_and_reports_unknown_fields` feeds two).
- `state_type_strings_are_recorded_linear_types` checks every filter/write
  string against the recorded `workflowStateTypes`;
  `every_recorded_state_type_maps_deliberately` fails when the recorded list's
  length changes — a future Linear state type is a test failure, not a silent
  `_ => Backlog` fallthrough.
- Accepted caveat (architect follow-up 4, re-confirmed): the fixture records
  **output** types only, so the create mutation's `$teamId: String` input
  typing is unverifiable offline. If wrong, `issue create` fails loud with a
  GraphQL validation error (`gql` returns `errors[0].message`), never silently.

**Errors surface on user-invoked paths — verified in code and live.** All three
§1.8 surfaces are consistent:

- `cmd/issue.rs::list_tracker_issues`: per-account `Err` → stderr warn
  (`{provider}/{account}: {e}`), all-failed → `msg::die`, exit non-zero.
  `--json` keeps stdout pure.
- `daemon/service.rs::issues_list`: per-account warn, all-failed →
  `ControlError::Internal` with the joined causes; partial success returns what
  worked.
- `hydrate_tracker.rs`: the silent error arm gained one `tracing::warn!` (the
  in-memory WARN+ ring holds it even with `THEGN_LOG` unset); panel semantics
  unchanged.
- In `linear.rs::gql` every failure arm returns `Err`: non-2xx → `Auth`
  (401/403) or `Api`; GraphQL `errors` → `Api(first message)`; no `data` →
  `Parse`. Nothing maps an error to an empty list anywhere.

Live (this branch's binary, real Linear, read-only):
`issue list --limit 5 --json` → real issues, exit 0. All five `--status`
filters return **only** that status — the §1.2 read/filter symmetry proven
against the real API again. With a throwaway `--config` carrying an invalid
token (user config untouched): `linear/probe: auth: HTTP 401 Unauthorized from
Linear API` on stderr, then `list issues failed: every configured account
errored (see above)`, exit 1. No empty-list lie, exit 0, or token echo.

**SecretRef resolution fails closed and never echoes — verified.**

- `resolve_account_token` parses once with `BareAs::Literal`; empty/blank,
  `keyring:` with no resolver, unset `env:`, unreadable/empty `file:` all →
  `None`, never the ref string. `keyring_ref_is_never_the_literal_string`
  asserts the THE-72 regression in both install states (order-independent hook).
- Value-free diagnostics only: every `warn!` names `SecretRef::audit_name()`
  (`keyring:<account>`), and core's `LiteralSecret` has redacted `Debug`, no
  `Display`, no `Serialize` (`thegn_core::secretref.rs`). Mechanical sweep for
  `api_key`/`api_token` in any `format!`/`tracing` across the branch files: none.
- `LinearBackend` holds the key privately (no `Debug` derive); the only use is
  the `Authorization` header. An unresolvable token becomes an empty key → the
  request 401s loudly — fails closed and loud, not silent.
- Host installs the resolver on **both** launches (`main.rs::run_subcommand`
  and `run.rs::main` — the interactive path the architect-review found missing),
  idempotently (`OnceLock`, first wins). The chunk-2 contract sharpening
  (hook receives the canonical `keyring:<account>` ref, not a bare account that
  `resolve_for` would misread as an env-var name) is correct and test-pinned.

**Bare-id routing / injection — verified.**

- GraphQL: `build_get_query`, `update_issue`'s identifier, `update_issue`'s
  title, and `build_list_query`'s `team_id` all go through
  `escape_graphql_str` (backslash first; `"`, `\n`, `\r`, `\t`). The search
  term and every create value go via GraphQL **variables**. Escaping is
  unit-tested including a control-API-shaped hostile id
  (`user_controlled_identifiers_cannot_break_out_of_the_literal`); a legitimate
  id is byte-identical. The live filters double as acceptance proof that
  escaped/multi-value documents parse on the real API.
- `wt new --from-issue`: the id flows `get_issue` → `issue_branch_seed`
  (tracker `branchName`, trimmed) → `worktree::dedupe` → git **argv** (no
  shell), and git's own refname rules reject `..`, `~^:?*[\`, control chars —
  traversal/option-injection is not reachable. The branchName-verbatim concern
  stands as a data-quality follow-up (architect-review §7.1), pre-existing and
  shared with the TUI `D` key; correctly left for its own issue.

**HTTP timeouts — present.** `Client::builder().timeout(15s).connect_timeout(5s)`
in `LinearBackend::new`, with a documented `unwrap_or_default` fallback.

**Dev-channel gating — intact, untouched.** The diff does not touch
`config_resolve.rs`; `clamp_to_channel` drops Linear/Jira/Kaneo accounts on
stable (GitHub kept) and `clamp_to_channel_is_a_noop_in_dev` + the
`config_resolve` suite pass. Every live probe above needed `THEGN_CHANNEL=dev`.

## 2. Tests — failure paths covered, gaps closed where cheap

Scoped runs, all green on the post-merge tree:

- `cargo nextest run -p thegn-svc linear` → 19 passed (12 contract tests).
- `cargo nextest run -p thegn-svc issue` → 62 passed (routing, id_miss,
  dedupe, foreign-prefix fallback, plugin round-trip).
- `cargo nextest run -p thegn-host secret` → 5; `-p thegn-host issue` → 9;
  `-p thegn-host daemon::service` → 20; `-p thegn-host ratchet` → 12.
- `cargo nextest run -p thegn-core clamp / channel / config_resolve` → 67 passed.
- `just quick thegn-svc`, `just quick thegn-host`,
  `cargo clippy -p thegn-svc --tests -D warnings`,
  `cargo clippy -p thegn-host --tests -D warnings` → clean.
- Ratchets re-run in the dev shell: ignored-result (after the §3 fix),
  async-trait, forge-leak, json-emit, element — all clean. The branch adds no
  allowlist entries of its own.

Coder "Unverified" items addressed: cross-crate compile proven (host+svc+core
build clean post-merge); live list/filter/error paths run by this review from
this branch's binary. Remaining unverified-by-design, both loud-failing if
wrong and each carrying an existing follow-up: `issue create` (input-type
fixture caveat — would mutate Linear, so not exercised) and
`wt new --from-issue` end-to-end (creates a local worktree — not read-only, so
not exercised; its `get_issue` document is unit-pinned and the escape is
tested). `keyring:` through the OS store end-to-end also remains wire-only
(fake hook in tests; the host path is read-verified on both launch paths).

## 3. Review fix committed

`d694e24e` — `fix(the-72): pin config_source.rs in the ignored-result ratchet
(review)`. Not a THE-72 defect: main's THE-83 landed
`crates/thegn-host/src/config_source.rs` with two deliberate ignored Results
(idempotent `OnceLock::set` install; discarded `clamp_to_channel` report on the
daemon's fresh re-load) while the gate was red, unpinned and unannotated — the
mandated merge-main-first surfaced it into this tree, where `just lint` failed
on it. Both sites now carry the `// best-effort:` annotation CLAUDE.md asks
for, and the file is pinned with a reason in
`test/ignored-result-ratchet.txt`, restoring `just lint` here (and on any
fresh main worktree).

## 4. Non-blocking findings (all pre-existing or already-tracked follow-ups)

- `issue_branch_seed` uses the tracker's `branchName` verbatim (architect
  follow-up 1) — see §1; low severity, argv-only, git-refname-guarded; deserves
  its own issue since THE-72 makes the Linear data path live in practice.
- `parse_tracker_statuses` silently drops unknown names, so
  `--status todoo` returns an unfiltered list (architect follow-up 2) —
  confident-wrong-answer family; pre-existing, out of THE-72's file set.
- No cursor pagination: >250 matching issues per account truncate silently
  (architect follow-up 3, observed live).
- `secret migrate` still files issue tokens (`store_file`) rather than keyring
  (architect follow-up 6) — comment updated by the branch; behaviour correct.
- An account whose token cannot resolve yields a 401-shaped error rather than
  a "no token for account X" message — loud but slightly less actionable;
  cosmetic follow-up only.
- `LinearIssue.priority: i64` against the fixture's `Float`: fine for whole
  numbers (live-verified); a fractional priority would fail deserialization
  loudly (Parse), never silently.

## 5. Frame-affecting changes (e2e note)

None. No panel/chrome/render file is in the diff; `hydrate_tracker.rs` adds a
log line in an error arm only; CLI and control-API output is text/JSON, not a
frame. No e2e re-record needed.

## 6. Merge-queue readiness

Branch is green on every gate this lane is allowed to run (scoped nextest,
clippy with tests, `just quick` both crates, ratchets, treefmt-stable tree,
live read-only probes). The full-workspace gates remain the pre-push/fold
gate's job per the dev-loop policy. Ready for `thegn integrate`.
