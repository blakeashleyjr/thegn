# THE-72 — architect review verdict

Branch `tg/the-72-linear-backend`, reviewed against
`.thegn/pipeline/THE-72/architect/design.md` and repo standards.

APPROVED

Four defects found and fixed in review (`6eecf7e7`); no revision chunk.

---

## 1. Merge

`git merge main` — **already up to date**, nothing to resolve. Reviewed
`git diff main...HEAD` in full: 8 code files + 2 new siblings + 1 new fixture +
`config/config.toml.example`, plus the lane docs.

## 2. Design conformance

Every numbered defect in the design is implemented as specified.

| design    | claim                                                                        | verified                                            |
| --------- | ---------------------------------------------------------------------------- | --------------------------------------------------- |
| §1.1      | singular `assignee`, one `ISSUE_FIELDS`, domain `Vec` unchanged              | ✅ code + live                                      |
| §1.2      | `backlog` mapped; `Backlog` filters `["backlog","triage"]`; writes `backlog` | ✅ code + live                                      |
| §1.3      | `canceled` (one `l`) on read/filter/write                                    | ✅ code + live                                      |
| §1.4      | argument `Vec` join — no `issues(, …)` parse error                           | ✅ code + live                                      |
| §1.5      | `type: { in: ["…"] }` bare strings                                           | ✅ code + live                                      |
| §1.6      | `first: 0` ⇒ 250 via shared `page_size`                                      | ✅ code + live                                      |
| §1.7/§2.4 | typed `SecretRef` resolution + `OnceLock` keyring hook                       | ✅ code + live (fail-closed path)                   |
| §1.8/§2.5 | CLI uses `list_per_provider`, warns + non-zero exit                          | ✅ code + live                                      |
| §1.9      | bare-id routes to the sole backend; `id_miss` names form + providers         | ✅ code + unit                                      |
| §2.2      | pure builders, no `self`, no I/O                                             | ✅ (+ `build_create_mutation`, a correct deviation) |
| §2.3      | recorded-schema contract test, 12 tests                                      | ✅ (see the caveat below)                           |
| §2.6      | out-of-scope items untouched                                                 | ✅ `workflowStates` team-scoping unchanged          |

Both coder deviations from the spec are **correct and better than the spec**:

- Chunk 1's fourth builder `build_create_mutation()` — without it the spec's
  test 4 could only re-derive the string and would assert nothing.
- Chunk 2's contract sharpening: the design said the hook receives the _account_
  while `main.rs` installs `resolve_for`, which parses with `BareAs::EnvName` —
  a bare `work-linear` would have read the **environment variable**
  `work-linear`. Passing the canonical `keyring:<account>` ref instead is right,
  is documented on `install_keyring_resolver`, and is asserted by a test. This
  was a bug in my design; the coder caught it.

## 3. Live verification against real Linear

A `file:` api_key is configured on this box, so the read paths were exercised
against the real API (read-only; **no issue was created or modified**).

```
THEGN_CHANNEL=dev thegn issue list --limit 5 --json      → real issues, exit 0
--status backlog|todo|in_progress|done|cancelled --json  → 275 / 74 / 19 / 282 / 86
```

Each status filter returns **only** issues of that status — which is the §1.2
symmetry requirement (read map and filter map agree) proven empirically, not
just against the fixture. `--limit 3|10|260` clamps correctly (two Linear
accounts are configured, so the per-account `first: 250` cap is what the 275 is
made of; see follow-up 3).

Error surfacing, with a deliberately invalid key:

```
thegn: linear/linear: auth: HTTP 401 Unauthorized from Linear API
thegn: list issues failed: every configured account errored (see above)
EXIT=1
```

That is exactly the §1.8 fix: previously `No issues found`, exit 0.

A `keyring:` ref for a non-existent account fails closed — no token, no literal
`keyring:…` sent as a bearer, and the account name appears nowhere in state.

## 4. Findings fixed in review (`6eecf7e7`)

1. **`run.rs` never installed the keyring resolver.** `main.rs:1048` is inside
   `run_subcommand`; the **interactive launch does not go through it**. So the
   TUI — where `hydrate_tracker` lives, i.e. the surface a `keyring:` tracker
   token exists for — kept resolving to nothing. My design named only `main.rs`
   and was wrong. Added the matching install at `run.rs:575`.
2. **`daemon/service.rs::issues_list` still swallowed.** It called
   `IssueRouter::list_issues`, which never returns `Err`, so the control-API
   `issues.list` verb — the capability catalog literally calls it "the door a
   supervisor lists its next batch through" — still answered an HTTP 400 with a
   confident empty list. That is §1.8's bug on a second surface the design did
   not enumerate. Rewritten per-account, matching `cmd/issue.rs`: partial
   failure warns and returns what worked, all-failed errors.
3. **GraphQL injection via the issue identifier.** `build_get_query` and
   `update_issue` spliced the user-controlled id raw into the document, and
   `build_list_query` did the same with the configured `team_id`. `issues.get` /
   `issues.update` are `SurfaceSet::ALL` verbs, so the id is not necessarily a
   local user's own string; a bare `"` closed the literal and grafted
   selections onto a document sent with the user's Linear token. Now escaped via
   the file's existing `escape_graphql_str`, with a regression test. A
   legitimate `ABC-123` is byte-identical to before. Chunk 1 flagged this for
   the reviewer rather than deciding it — correct call, and the answer is yes.
4. **`issue/secret.rs` failed the ignored-`Result` ratchet.** Two `let _ =`
   (chunk 2's done-doc asserted none). Fixed rather than pinned — the allowlist
   only shrinks: `OnceLock::get_or_init` for the install, an `expect` in the
   test.

Plus the now-stale "keyring for these lands with the svc resolver" comment in
`cmd/secret.rs` (chunk 2 correctly left it out of its file list). Comment only —
`secret migrate` still writes `file:`, which remains correct and is the form
that resolves on a headless box.

## 5. Audit items from the lead

| item                                                        | result                                                                                                                                                                                                                                                                                                                                                                                      |
| ----------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| contract test pins real field/type strings, not a tautology | **Pass, with a caveat.** The checker walks `ISSUE_FIELDS` against an independent JSON fixture and pins `Issue.assignee == "User"` and the absence of `assignees`. Not self-referential. Caveat: the fixture is hand-transcribed from my design, not machine-recorded from a live introspection — its `_provenance` says so. The live run above is the stronger evidence for the read paths. |
| errors surfaced on the user-invoked primary path            | **Pass after fix 2.** CLI proven live (exit 1 + stderr). `view`/`create`/`comment` already `msg::die`. `IssueRouter::list_issues` keeps its swallow contract for the background fan-out only, now documented as such; `hydrate.rs`/`hydrate_tracker.rs` are deliberately best-effort and `hydrate_tracker`'s previously-silent arm gained a warn.                                           |
| token never logged or echoed                                | **Pass.** Every diagnostic names `SecretRef::audit_name()`, which is value-free by construction (`Literal` renders as a placeholder). No `Display`/`Debug` of a value anywhere in the diff.                                                                                                                                                                                                 |
| never passes a literal `file:`/`env:` string as a bearer    | **Pass.** Typed `SecretRef::parse(.., BareAs::Literal)` — the scheme arms resolve; only a genuinely bare string is used verbatim, which is that field family's documented historic meaning. `keyring:` never falls through to the literal arm (pinned by `keyring_ref_is_never_the_literal_string`).                                                                                        |
| fails closed                                                | **Pass.** Unresolvable ⇒ `None` ⇒ empty key ⇒ 401, which is now surfaced rather than swallowed. Verified live.                                                                                                                                                                                                                                                                              |
| hostile issue id can't inject                               | **Pass after fix 3** for GraphQL. Shell: nothing in this path shells out. Path/URL: the id is not used to build a path; the branch name comes from `issue_branch_seed` over the _server's_ `branchName`, unchanged by this diff — see follow-up 1.                                                                                                                                          |
| timeouts on the HTTP call                                   | **Pass, pre-existing.** 15 s request / 5 s connect, set on `main` already; untouched.                                                                                                                                                                                                                                                                                                       |
| dev-channel gated                                           | **Pass, untouched.** Gating is at the config layer (`config_resolve.rs`, `Feature::Trackers` clamps non-GitHub trackers out on stable); the diff does not touch it. Every live check above needed `THEGN_CHANNEL=dev`.                                                                                                                                                                      |

## 6. Gates run

```
cargo nextest run -p thegn-svc issue    → 62 passed   (13 new incl. the escaping test)
cargo nextest run -p thegn-host issue   →  7 passed
cargo nextest run -p thegn-host daemon::service → 20 passed
cargo nextest run -p thegn-host ratchet → 12 passed
cargo clippy -p thegn-svc --tests       → clean
just quick thegn-host / thegn-svc       → clean
ratchet.sh ignored-result/async-trait/forge-leak/json-emit/element → clean
brand-guard / stale-docs-guard          → clean
treefmt (touched files)                 → clean
just smoke (+ pty-smoke)                → all passed, 3m32s
```

Per the lead's budget: no `just test` / `just ci` / `just coverage` / e2e.
Not run, therefore not proven: full-workspace clippy, cross/feature/MSRV,
coverage, nix-build, openspec-validate. The `thegn-core` coverage gate is
unaffected — core is untouched by this branch.

## 7. Follow-ups (out of scope, none blocking)

1. `issue_branch_seed` uses the tracker's `branchName` **verbatim** (trim only;
   only the fallback path slugifies), so remote data reaches a git branch name.
   Pre-existing on `main` and reachable before this change via `linear:THE-72`,
   so THE-72 neither introduces nor widens it — but it is worth its own issue.
2. `parse_tracker_statuses` silently drops unknown names, so
   `thegn issue list --status todoo` returns an **unfiltered** list. Same
   confident-wrong-answer family as §1.8; pre-existing.
3. No cursor pagination: an account with more than 250 issues in a filter is
   silently truncated (observed live — the 275 above is two accounts, not one
   account's full backlog).
4. The create mutation declares `$teamId: String` against Linear's
   `IssueCreateInput.teamId`. The fixture records **output** types only, so the
   contract test cannot see input types, and `create` was not exercised live
   (it would have created an issue). Worth confirming before relying on
   `issue create`.
5. Refresh `linear_schema.json` from a real introspection dump the next time
   someone has one, and note the command in `_provenance`.
6. `secret migrate` could now route issue tokens to `store` (keyring) instead of
   `store_file`; deliberately left alone (see §4).
