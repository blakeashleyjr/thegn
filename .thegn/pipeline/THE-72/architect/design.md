# THE-72 — Linear issue backend has never worked

**Architect design.** Branch `tg/the-72-linear-backend`.
Issue: <https://linear.app/blakeashley/issue/THE-72>

---

## 1. What is actually broken

The issue reports three defects plus a secret-resolution note. Reading the code
turned up **six** query-level defects (three reported, three new) and corrected
one premise. Every claim below cites the line it is read from.

### 1.1 `assignees` is not a field on Linear's `Issue` — CONFIRMED (reported)

`crates/thegn-svc/src/issue/linear.rs:294-301`

```rust
const ISSUE_FIELDS: &str = r#"
    id identifier title description
    state { type }
    priority
    assignees { nodes { name } }      // ← Issue.assignee is singular
    labels { nodes { name } }
    branchName dueDate url updatedAt
"#;
```

Linear's `Issue` type exposes `assignee: User` (nullable, singular). Selecting
`assignees` is a **document validation** failure, so Linear rejects the whole
document with `GRAPHQL_VALIDATION_FAILED` / HTTP 400 before executing anything.

`ISSUE_FIELDS` is interpolated into **every** read path —
`list_issues` (`:352`), `get_issue` (`:374`), `update_issue` (`:505`),
`search` (`:531`) — and the create mutation carries a _hand-duplicated copy_ of
the same selection at `:425` (`assignees { nodes { name } }` again). So the
whole backend is dead, not one verb.

Deserialization side: `LinearIssue.assignees: Option<LinearUserList>`
(`:116`) and `linear_issue_to_domain` (`:270-276`) must singularize with it.
The domain type stays `Issue.assignees: Vec<String>`
(`crates/thegn-core/src/issue.rs`) — that is the provider-agnostic shape, and
Jira already collapses its own singular `assignee` into it
(`crates/thegn-svc/src/issue/jira.rs:290-292`). **Do not change the domain type.**

### 1.2 `Backlog → "triage"` — CONFIRMED (reported)

Read side `:206`, list-filter side `:325`, write side `:461`.

Linear's workflow-state `type` values are exactly:
`triage`, `backlog`, `unstarted`, `started`, `completed`, `canceled`.
`backlog` is simply absent from the mapping today, so a real backlog issue falls
through `map_state`'s `_ => IssueStatus::Backlog` fallback (`:211`) — right
answer by accident on the read side, wrong on the filter/write side.

**Decision — `triage` merges into `Backlog`.** The domain has five statuses
(`IssueStatus`, `crates/thegn-core/src/issue.rs:46-48`) and gains nothing from a
sixth that only one provider has. But the merge has to be _symmetric_: if
`map_state` labels a `triage` issue `Backlog`, then filtering by `Backlog` must
request **both** `backlog` and `triage`, or `thegn issue list --status backlog`
silently omits issues the unfiltered list shows as Backlog. Hence the reverse
map returns a **set** of Linear type strings, not one:

| `IssueStatus` | read (Linear → domain) | filter (domain → Linear) | write target |
| ------------- | ---------------------- | ------------------------ | ------------ |
| `Backlog`     | `backlog`, `triage`    | `["backlog", "triage"]`  | `backlog`    |
| `Todo`        | `unstarted`            | `["unstarted"]`          | `unstarted`  |
| `InProgress`  | `started`              | `["started"]`            | `started`    |
| `Done`        | `completed`            | `["completed"]`          | `completed`  |
| `Cancelled`   | `canceled`             | `["canceled"]`           | `canceled`   |

The **write** direction stays single-valued (`issueUpdate` needs one
`stateId`), and Backlog writes to `backlog` — `triage` is an intake queue, not
somewhere thegn should push an issue.

### 1.3 `Cancelled → "cancelled"` — CONFIRMED (reported)

`:210` (read), `:328` (filter), `:465` (write). Linear spells it `canceled`
(one `l`, US spelling). The read side means a cancelled Linear issue is reported
as `Backlog` via the fallback; the filter/write sides send a string Linear does
not know.

### 1.4 NEW — an unfiltered list query is a GraphQL **syntax** error

`:343-354`

```rust
let filter_block = if conditions.is_empty() { String::new() } else { … };
let query = format!(r#"query {{ issues({filter_block}, first: {limit}, orderBy: updatedAt) {{ … "#);
```

With no `assignee_me`, no statuses and no `team_id` the emitted document is
`issues(, first: 0, orderBy: updatedAt)` — a **leading comma inside an argument
list**, which is a parse error, not merely a validation one. This is the default
shape of `thegn issue list --limit N` on a config with no `team_id`
(`crates/thegn-core/src/config_issues.rs:236-248` defaults `team_id` empty).

Fix: build the argument list as a `Vec<String>` and join it, so an absent
`filter` contributes nothing rather than an empty slot.

### 1.5 NEW — `type: { in: [{ eq: "…" }] }` is the wrong comparator shape

`:331-336`

```rust
let types_str = types.iter().map(|t| format!(r#"{{ eq: "{t}" }}"#)).collect::<Vec<_>>().join(", ");
conditions.push(format!("state: {{ type: {{ in: [{types_str}] }} }}"));
```

`WorkflowStateFilter.type` is a `StringComparator`, whose `in` field is
`[String!]` — a list of **strings**, not a list of nested comparators. The
emitted `in: [{ eq: "unstarted" }]` fails validation. Correct form:

```graphql
state: { type: { in: ["backlog", "triage"] } }
```

So `thegn issue list --status …` is broken independently of §1.1 and would stay
broken after fixing only the reported three.

### 1.6 NEW — `first: 0` on the default CLI path

`:349` `let limit = filter.limit.min(250);` and `:526` in `search`.

`IssueFilter::limit` is `0` by default (`crates/thegn-core/src/issue.rs:149`,
plain `#[derive(Default)]`), and `crates/thegn-host/src/cmd/issue.rs:76`
passes `limit.unwrap_or(0)` — so `thegn issue list --status todo` (no
`--limit`) asks Linear for `first: 0`, which is out of the 1..=250 range the
pagination arguments accept.

`0` means "no cap" to the caller (`cmd/issue.rs:157` only truncates when
`limit > 0`), so it must map to the page maximum, not to 1:

```rust
let first = if filter.limit == 0 { 250 } else { filter.limit.min(250) };
```

### 1.7 CORRECTED PREMISE — `file:` refs already work; `keyring:` is the real gap

The issue states `expand_env_ref` "resolves only `env:` refs". It does not:
`crates/thegn-core/src/config.rs:82-100` handles `env:`, `file:` (with `~`
expansion) **and** a bare literal. The genuine hole is `keyring:` — the fourth
`SecretRef` variant (`crates/thegn-core/src/secretref.rs:86-95`) — which falls
into the bare-literal arm and is handed to Linear as the literal API key
`"keyring:my-linear"`, producing an opaque auth failure.

This is _known, documented debt_, not a surprise —
`crates/thegn-host/src/secret.rs:150-155`:

> Used by `secret migrate` for fields whose runtime resolution does not yet go
> through the keyring-capable broker (issue/CI tokens still resolve via
> `expand_env_ref`, which handles `env:`/`file:` but not `keyring:`) … until the
> svc resolver injection lands.

THE-72 lands that injection for the issue-tracker family. See §2.4.

### 1.8 NEW (audit item from the issue) — a 400 becomes a silent empty list

`crates/thegn-svc/src/issue/mod.rs:290-301`:

```rust
pub async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
    for b in &self.inner {
        match b.inner.list_issues(filter).await {
            Ok(mut issues) => all.append(&mut issues),
            Err(e) => tracing::warn!(…, "issue list failed"),   // swallowed
        }
    }
    Ok(all)                                                     // always Ok
}
```

`crates/thegn-host/src/cmd/issue.rs:153-166` then prints `No issues found` and
exits 0. With **no `THEGN_LOG` set no sink is installed at all** (CLAUDE.md,
Performance invariants), so the `tracing::warn!` goes nowhere: the user sees a
clean, confident, wrong answer. That violates the repo rule that errors on the
primary path of a user-invoked action must surface (CLAUDE.md, Conventions).

Same hole, quieter, in `crates/thegn-host/src/hydrate_tracker.rs:80-87`: a
failing account `continue`s **without even a warn** (only transient errors touch
the connectivity holder), so the panel shows an empty tracker forever.

The best-effort merge is right for the _background_ fan-out (one bad account
must not blank the other three) — so keep `list_issues` as it is and let the
**CLI** use the per-account results that already exist
(`IssueRouter::list_per_provider`, `mod.rs:305-318`).

### 1.9 NEW — `wt new --from-issue THE-72` cannot route

`IssueRouter::backend_for_id` (`mod.rs:280-286`) splits the id on `:` and
matches the prefix against `provider_id()`. A bare `THE-72` yields prefix
`"THE-72"`, matches nothing, and returns `IssueError::NotConfigured`, which
`cmd/wt.rs:483-485` renders as
`fetch issue THE-72: no issue provider configured` — on a machine where a
provider very much _is_ configured. The clap help does document the
`"<provider>:<key>"` form (`cmd/wt.rs:104-108`), so this is user error with a
lying error message.

Cheap, unambiguous fix: when the id carries no known provider prefix **and
exactly one backend is configured**, route to it; otherwise fail with a message
that names the expected form and the configured providers. Two-or-more backends
stay strict — guessing across accounts would be worse than the error.

---

## 2. Design

### 2.1 Invariants this change must respect

- **`thegn-core` stays substrate-free** — no reqwest/GraphQL knowledge moves
  into core. All of §1.1–1.6 lives in `thegn-svc`.
- **Provider-seam shape unchanged** — `IssueBackend` keeps its `BoxFuture`
  methods (the `async fn`-in-provider-trait ratchet, `test/*-ratchet.txt`); no
  trait method is added or removed.
- **No new `let _ =`** without a `// best-effort:` reason (ignored-`Result`
  ratchet).
- **0% idle / no blocking I/O on the loop** — untouched; every call here is
  already on a background runtime or a CLI thread.
- **Coverage gate is `thegn-core`-only at 95%** — the new logic is in `svc`, so
  it is not gated, but it is _pure and unit-tested_ by construction (§2.3).
- **Nix source allowlist**: the new fixture lives under `crates/`, which is an
  allowlisted root (`nix/source.nix:23-34`) — no `source.nix` edit needed.
  Do not put it anywhere else.
- **Feature-gating unchanged**: trackers are `Feature::Trackers`, Experimental
  (`crates/thegn-core/src/channel.rs:96-130`); manual verification needs
  `THEGN_CHANNEL=dev`.

### 2.2 Make the query strings pure and testable

The single reason six defects shipped is that no query string is reachable
without a network call. Extract the document builders as free functions with no
`self` and no I/O:

```rust
fn build_list_query(filter: &IssueFilter, team_id: Option<&str>) -> String
fn build_search_query(query: &str, limit: usize) -> String
fn build_get_query(identifier: &str) -> String
fn issue_selection() -> &'static str          // ISSUE_FIELDS, one copy
```

and have `create_issue`/`update_issue` interpolate `ISSUE_FIELDS` instead of
carrying the hand-duplicated copy at `:425` (note the `format!` brace-doubling
`{{`/`}}` this needs for the surrounding GraphQL object literals). One
selection, one place to be wrong.

State mapping becomes two explicit functions, both pure:

```rust
/// Domain status → every Linear state `type` that means it (read/filter side).
fn status_to_state_types(s: IssueStatus) -> &'static [&'static str];
/// The single canonical write target for `issueUpdate`'s stateId lookup.
fn status_to_write_state_type(s: IssueStatus) -> &'static str;
```

### 2.3 The contract test: pin the selection against a recorded schema

A recorded fixture, not a live call — the suite must stay hermetic and offline.

- **Fixture**: `crates/thegn-svc/src/issue/linear_schema.json`, a trimmed
  introspection record covering only the types thegn selects from (`Issue`,
  `WorkflowState`, `User`, `IssueLabel`, `Comment`, and the three connection
  types), plus the closed list of `workflowStateTypes`. It carries a
  `_provenance` field naming the source and the date it was recorded, so the
  next person knows how to refresh it.
- **Checker + tests**: `crates/thegn-svc/src/issue/linear_schema_tests.rs`,
  wired from `linear.rs` with the repo's established sibling-test idiom
  (`crates/thegn-core/src/config.rs:6885` — `#[cfg(test)] #[path = "…"] mod …;`)
  so it can see the private constants. `include_str!("linear_schema.json")`
  resolves relative to the test file, which sits beside the fixture.
- The checker is a ~40-line selection-set tokenizer: split a selection into
  `name` / `name { sub }` pairs, look each `name` up on the recorded type, and
  recurse into the sub-selection against the field's recorded type.

Tests it must carry:

1. every field selected by `ISSUE_FIELDS` exists on the recorded `Issue`
   (recursively) — **and an explicit assertion that the recorded `Issue` has no
   `assignees` field**, which is the THE-72 regression itself;
2. the `comments { nodes { … } }` sub-selection of `get_issue` validates;
3. the create/update mutation selection validates (it must be the _same_ string
   as `ISSUE_FIELDS` after §2.2 — assert that too);
4. every string produced by `status_to_state_types` /
   `status_to_write_state_type` is in the recorded `workflowStateTypes`;
5. every recorded `workflowStateTypes` entry maps through `map_state` to its
   documented status (so a Linear-side addition shows up as a failing test, not
   as silent `_ => Backlog`);
6. `build_list_query` with an empty `IssueFilter` and no `team_id` contains no
   `(,` / `, ,`, has balanced braces, and asks for `first: 250` — the §1.4/§1.6
   regressions;
7. `build_list_query` with `statuses = [Backlog]` emits
   `type: { in: ["backlog", "triage"] }` — bare strings, both values (§1.2/§1.5).

### 2.4 Typed token resolution (`SecretRef`) with a host-installed keyring hook

`crates/thegn-svc/src/issue/mod.rs:157/167/176` all call
`expand_env_ref(&a.token).unwrap_or_default()`. Replace with one svc-local
resolver in a new `crates/thegn-svc/src/issue/secret.rs`:

```rust
pub(crate) fn resolve_account_token(raw: &str, provider: &str) -> Option<String>
```

which parses `SecretRef::parse(raw, BareAs::Literal)` — `Literal` is the correct
marker for this field family, documented at `secretref.rs:33-42` and matching
the historic tracker-token meaning — and then:

| variant               | resolution                                                        |
| --------------------- | ----------------------------------------------------------------- |
| `Env { var }`         | `std::env::var`, non-empty                                        |
| `File { path }`       | read + trim, non-empty (`~` via `thegn_core::util::expand_tilde`) |
| `Literal(v)`          | `expose()`                                                        |
| `Keyring { account }` | the installed hook, else `None` + one actionable `tracing::warn!` |

**Why a hook and not a threaded closure.** `hibernator.rs:112` threads a
resolver closure (`thegn_svc::snapshot::open_store(cfg, &|r| crate::secret::resolve(r))`),
which is the nicer shape — but `IssueRouter::from_config{,_at}` has **12 call
sites** across host (hydrate, hydrate_tracker, handlers/tracker, daemon/service
×5, cmd/issue, cmd/wt, cmd/kaneo), and threading a parameter through all of them
is churn disproportionate to a bugfix. Use the repo's other established shape —
a process-global installed once at startup, exactly like
`crates/thegn-host/src/forge_handle.rs:17-24`:

```rust
// thegn-svc
pub fn install_keyring_resolver(f: fn(&str) -> Option<String>);   // OnceLock, idempotent
```

installed from `crates/thegn-host/src/main.rs` beside
`crate::forge_handle::install(&cfg)` (`main.rs:1045`), which runs **before the
subcommand match** and therefore covers every verb including `daemon`/`serve`.
A process that never installs (any unit test, any svc-only consumer) keeps
today's behaviour minus the bogus literal — the same fail-safe posture as
`sandbox_cpucap::publish_background_limits` (`main.rs:1050-1056`).

Then delete the stale caveat in `crates/thegn-host/src/secret.rs:150-155` and
document the four accepted forms for `[issues.*] api_key` in
`config/config.toml.example:597-616` (it currently only shows `env:`).

**Never log the value**: the warn names `r.audit_name()`
(`secretref.rs:159-166`), which is value-free by construction.

### 2.5 Surfacing the error (§1.8)

- `cmd/issue.rs::list_tracker_issues` switches from `router.list_issues` to
  `router.list_per_provider` (already public, `mod.rs:305`): print every
  per-account failure to stderr via `thegn_core::msg::warn`
  (`crates/thegn-core/src/msg.rs:60`), and `msg::die` when **every** configured
  account failed — a total failure must be a non-zero exit, never
  `No issues found`. A partial failure warns and prints what did come back.
- `hydrate_tracker.rs:80-87` gains a `tracing::warn!` on the error arm (it has
  none today). The panel's best-effort semantics are unchanged.
- `IssueRouter::list_issues` keeps its swallow-and-merge contract for the
  background fan-out; its doc comment says so explicitly and points at
  `list_per_provider` for callers that must report.

### 2.6 Explicitly out of scope

- `update_issue`'s `workflowStates(filter: …, first: 1)` picks the first state
  of a type **across all teams** (`linear.rs:467-474`) — pre-existing, already
  commented as best-effort, and orthogonal. Fix the type _string_ it passes;
  leave the team-scoping alone.
- Multi-account `get`/`update` disambiguation beyond §1.9's single-backend case
  (`mod.rs:277-279` documents the limitation).
- Any change to `IssueStatus` or the `Issue` domain type.
- Jira/Kaneo/GitHub backends, except that they inherit §2.4's resolver.
- openspec: trackers have no `openspec/specs/<capability>/` of their own, and
  this is a defect fix inside existing behaviour — no delta spec.

---

## 3. Verification

Hermetic, offline, and cheap — no live Linear call in the suite.

```
just quick thegn-svc
just quick thegn-host
cargo nextest run -p thegn-svc linear
cargo nextest run -p thegn-svc issue
cargo nextest run -p thegn-host secret
```

Manual (dev channel, needs a real key — **not** part of the gate):

```
THEGN_CHANNEL=dev thegn issue list --status todo --limit 10
THEGN_CHANNEL=dev thegn issue list --limit 5 --json
THEGN_CHANNEL=dev thegn wt new --from-issue linear:THE-72
```

Do **not** run `just test` / `just ci` / e2e per-edit; the pre-push hook is the
heavy gate (CLAUDE.md, Dev-loop policy).

## 4. Chunks

| chunk | area                                                                      | files                                                                                            | parallel?                  |
| ----- | ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ | -------------------------- |
| 1     | Linear query correctness + recorded-schema contract test                  | `crates/thegn-svc/src/issue/linear.rs` (+2 new siblings)                                         | yes — file-disjoint from 2 |
| 2     | Router/CLI error surfacing + typed SecretRef resolution + bare-id routing | `crates/thegn-svc/src/issue/mod.rs` (+1 new sibling), 4 host files, `config/config.toml.example` | yes — file-disjoint from 1 |

Neither chunk edits a file the other edits, and neither depends on the other's
symbols: chunk 2 does not read `linear.rs`, and chunk 1 does not read
`issue/mod.rs`. They can land in either order.
