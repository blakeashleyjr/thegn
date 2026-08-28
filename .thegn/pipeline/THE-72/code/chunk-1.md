# THE-72 — chunk 1: Linear query correctness + recorded-schema contract test

Read `.thegn/pipeline/THE-72/architect/design.md` §1.1–1.6, §2.1–2.3 first.
This chunk fixes the six query-level defects that make every Linear call fail,
and adds the offline contract test that stops them coming back.

## Files touched (exact)

| path                                                | action                             |
| --------------------------------------------------- | ---------------------------------- |
| `crates/thegn-svc/src/issue/linear.rs`              | edit                               |
| `crates/thegn-svc/src/issue/linear_schema.json`     | **new** (recorded schema fixture)  |
| `crates/thegn-svc/src/issue/linear_schema_tests.rs` | **new** (checker + contract tests) |

Nothing else. Do **not** touch `crates/thegn-svc/src/issue/mod.rs`,
`crates/thegn-core/src/issue.rs`, or any host file.

## Overlap / dependency

**None with chunk 2** — chunk 2 touches `issue/mod.rs`, `issue/secret.rs` and
host files, and neither chunk reads the other's new symbols. Chunk 1 and chunk 2
are file-disjoint and can run in parallel, landing in either order.

## Approach

### A. One selection, singular assignee

1. `ISSUE_FIELDS` (`linear.rs:294-301`): `assignees { nodes { name } }` →
   `assignee { name }`.
2. `LinearIssue` (`:116`): `assignees: Option<LinearUserList>` →
   `assignee: Option<LinearUser>` (keep `#[serde(default)]`).
   `LinearUserList` is still used by nothing else — delete it if it becomes
   dead, keep `LinearLabelList`.
3. `linear_issue_to_domain` (`:270-276`):
   `assignees: li.assignee.map(|u| u.name).into_iter().collect(),`
   The domain field stays `Vec<String>` — Jira collapses the same way
   (`jira.rs:290-292`). **Do not change `thegn_core::issue::Issue`.**
4. The create mutation at `:423-427` carries a hand-duplicated copy of the
   selection. Replace it with an interpolation of `ISSUE_FIELDS` so there is
   exactly one selection string in the file (`format!` needs `{{`/`}}` for the
   surrounding GraphQL object literals; the `$title`/`$priority` variables are
   untouched by `format!`).

### B. State-type mapping (read / filter / write)

Replace the three ad-hoc `match`es at `:204-213`, `:322-330`, `:459-466` with:

```rust
/// Linear's workflow-state `type` values, folded onto the five domain
/// statuses. `triage` merges into `Backlog`: it is Linear's intake queue and
/// the domain has no separate status for it.
fn map_state(state: Option<&LinearState>) -> IssueStatus {
    match state.map(|s| s.state_type.as_str()) {
        Some("triage") | Some("backlog") => IssueStatus::Backlog,
        Some("unstarted") => IssueStatus::Todo,
        Some("started") => IssueStatus::InProgress,
        Some("completed") => IssueStatus::Done,
        Some("canceled") => IssueStatus::Cancelled,
        _ => IssueStatus::Backlog,
    }
}

/// Every Linear state `type` that reads back as `s`. Backlog covers both
/// `backlog` and `triage` so a `--status backlog` filter cannot drop issues the
/// unfiltered list labels Backlog.
fn status_to_state_types(s: IssueStatus) -> &'static [&'static str] { … }

/// The single canonical write target for `issueUpdate`'s stateId lookup —
/// Backlog writes to `backlog`, never to the `triage` intake queue.
fn status_to_write_state_type(s: IssueStatus) -> &'static str { … }
```

Table (design §1.2): Backlog `["backlog","triage"]`/`backlog`, Todo
`["unstarted"]`, InProgress `["started"]`, Done `["completed"]`, Cancelled
`["canceled"]` — note the **single `l`** in `canceled` everywhere.

### C. Pure query builders

Extract from the trait impl (no `self`, no I/O — this is what makes the
contract test possible at all):

```rust
fn build_list_query(filter: &IssueFilter, team_id: Option<&str>) -> String
fn build_search_query(query: &str, limit: usize) -> String
fn build_get_query(identifier: &str) -> String
```

`build_list_query` must fix three things at once:

- **Argument list** (`:343-354`): collect args into a `Vec<String>` and
  `join(", ")`. An absent `filter` contributes **nothing** — today an empty
  `filter_block` emits `issues(, first: …)`, a GraphQL _parse_ error, which is
  the default unfiltered CLI shape.
- **Comparator shape** (`:331-336`): `WorkflowStateFilter.type` is a
  `StringComparator` whose `in` is `[String!]`. Emit
  `state: { type: { in: ["backlog", "triage"] } }` — bare strings, **not**
  `[{ eq: "…" }]`. Flatten `status_to_state_types` across the requested
  statuses and de-duplicate while preserving order.
- **Page size** (`:349`, and `:526` in search): `limit == 0` means "no cap" to
  the caller (`cmd/issue.rs:157` only truncates when `limit > 0`), and Linear
  rejects `first: 0`. Use
  `let first = if limit == 0 { 250 } else { limit.min(250) };`.

Leave `assignee: { isMe: { eq: true } }` (`:316`) and
`team: { id: { eq: … } }` (`:340`) as they are — both are correct.
`escape_graphql_str` still guards the search term; the state-type strings are
static and need no escaping.

`update_issue`'s `workflowStates(filter: …, first: 1)` team-scoping bug
(`:467-474`) is **out of scope** (design §2.6) — only its type string changes.

### D. Recorded schema fixture

`crates/thegn-svc/src/issue/linear_schema.json` — a trimmed introspection
record of only the types thegn selects from. It must live under `crates/`
(an allowlisted nix source root, `nix/source.nix:23-34`); putting it anywhere
else breaks the sandboxed build.

```json
{
  "_provenance": "Linear GraphQL API (api.linear.app/graphql), recorded 2026-08-27 for THE-72. Refresh from a live introspection when Linear changes; the tests here are the drift alarm.",
  "types": {
    "Issue": {
      "id": "String",
      "identifier": "String",
      "title": "String",
      "description": "String",
      "priority": "Float",
      "branchName": "String",
      "dueDate": "TimelessDate",
      "url": "String",
      "createdAt": "DateTime",
      "updatedAt": "DateTime",
      "state": "WorkflowState",
      "assignee": "User",
      "creator": "User",
      "team": "Team",
      "labels": "IssueLabelConnection",
      "comments": "CommentConnection",
      "parent": "Issue",
      "children": "IssueConnection",
      "estimate": "Float",
      "number": "Float"
    },
    "WorkflowState": {
      "id": "String",
      "name": "String",
      "type": "String",
      "color": "String",
      "position": "Float"
    },
    "User": {
      "id": "String",
      "name": "String",
      "displayName": "String",
      "email": "String",
      "active": "Boolean"
    },
    "IssueLabel": { "id": "String", "name": "String", "color": "String" },
    "Comment": {
      "id": "String",
      "body": "String",
      "user": "User",
      "createdAt": "DateTime",
      "updatedAt": "DateTime"
    },
    "Team": { "id": "String", "key": "String", "name": "String" },
    "IssueLabelConnection": { "nodes": "IssueLabel" },
    "CommentConnection": { "nodes": "Comment" },
    "IssueConnection": { "nodes": "Issue" }
  },
  "workflowStateTypes": [
    "triage",
    "backlog",
    "unstarted",
    "started",
    "completed",
    "canceled"
  ]
}
```

The record is deliberately **not** exhaustive — it covers what thegn selects,
and an unknown field is a test failure, which is the point. Note there is no
`assignees` key on `Issue`; that absence is THE-72 itself.

### E. Contract test

`crates/thegn-svc/src/issue/linear_schema_tests.rs`, wired from the bottom of
`linear.rs` with the repo's sibling-test idiom (precedent:
`crates/thegn-core/src/config.rs:6885`):

```rust
#[cfg(test)]
#[path = "linear_schema_tests.rs"]
mod schema_contract;
```

so it reaches the private `ISSUE_FIELDS` / builders via `use super::*;`.
`include_str!("linear_schema.json")` resolves beside the test file.

The checker is a small selection-set tokenizer (~40 lines): split a selection
into `name` / `name { sub }` pairs, look each name up on the recorded type, and
recurse into the sub-selection against that field's recorded type. Report
**every** unknown field, not just the first.

Tests (all offline, no network, no fixtures outside the crate):

1. `issue_fields_selection_matches_recorded_schema` — every field in
   `ISSUE_FIELDS`, recursively, exists on the recorded `Issue`.
2. `recorded_issue_type_has_no_assignees_field` — asserts the absence
   explicitly, with a comment naming THE-72. This is the regression itself.
3. `get_issue_comments_selection_matches_recorded_schema` — the
   `comments { nodes { body user { name } createdAt } }` subtree.
4. `create_mutation_selects_the_same_fields_as_issue_fields` — the mutation
   string contains `ISSUE_FIELDS` (i.e. the duplicate from §A.4 is gone).
5. `state_type_strings_are_recorded_linear_types` — every string from
   `status_to_state_types` and `status_to_write_state_type` is in
   `workflowStateTypes`.
6. `every_recorded_state_type_maps_deliberately` — each of the six recorded
   types maps through `map_state` to its documented status, so a Linear-side
   addition fails loudly instead of falling into `_ => Backlog`.
7. `unfiltered_list_query_is_well_formed` — `build_list_query` with
   `IssueFilter::default()` and `team_id: None` contains no `(,` and no `, ,`,
   has balanced braces, and contains `first: 250`.
8. `backlog_filter_requests_both_backlog_and_triage` — a `[Backlog]` filter
   emits `type: { in: ["backlog", "triage"] }` (bare strings, both values).
9. `limit_clamps_to_the_page_maximum` — `limit: 0 ⇒ first: 250`,
   `limit: 10 ⇒ first: 10`, `limit: 9999 ⇒ first: 250`, for both
   `build_list_query` and `build_search_query`.

Keep the existing tests in `linear.rs`'s `mod tests` green — several assert the
old strings (`map_state_covers_all_types_and_fallback` at `:557` asserts
`"cancelled"`, `issue_to_domain_maps_all_fields` at `:618` feeds
`"assignees": { "nodes": [...] }`). **Update them**, don't delete them: the
first becomes the six real types, the second feeds `"assignee": { "name": … }`
and still asserts `issue.assignees == vec!["Fox Mulder"]`.

## Conventions to honour

- `thegn-svc` only — nothing moves into `thegn-core` (substrate-free invariant).
- Don't add or remove an `IssueBackend` trait method; the `BoxFuture` shape is
  ratcheted (`async fn` in provider traits, `test/*-ratchet.txt`).
- No new bare `let _ =` / `.ok()` without a `// best-effort: <why>` comment.
- Comment density and doc-comment voice: match `linear.rs` as it stands —
  short, reason-giving, no restating the code.

## Tests to run (scoped — never a full-workspace gate)

```
just quick thegn-svc
cargo nextest run -p thegn-svc linear
```

Do not run `just test`, `just ci`, `just coverage`, or e2e.

## Done criteria

- [ ] No `assignees` anywhere in a Linear GraphQL selection string; one
      `ISSUE_FIELDS`, used by list/get/create/update/search.
- [ ] `map_state` handles all six Linear types; `canceled` spelled with one `l`
      in every direction.
- [ ] `build_list_query` / `build_search_query` / `build_get_query` are pure
      free functions with no `self`.
- [ ] Unfiltered list query has no leading comma; `first` is always in 1..=250.
- [ ] Status filter emits `in: ["…"]` bare strings, Backlog expanding to
      `backlog` + `triage`.
- [ ] The 9 contract tests plus the updated existing tests pass under
      `cargo nextest run -p thegn-svc linear`.
- [ ] `just quick thegn-svc` clean (clippy `-D warnings`).
- [ ] Committed with **exactly** this subject:

```
fix(linear): singular assignee, real state types, valid list query (THE-72)
```
