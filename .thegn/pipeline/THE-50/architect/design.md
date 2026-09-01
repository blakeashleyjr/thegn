# THE-50 — tracker / issue-provider seam audit and design

## Decision

THE-50 is an audit plus the cheap seam-hardening work that the audit can prove
is local. It does not implement a new tracker suite. The branch has the
account-shaped `IssueBackend` seam, not the draft change's proposed
`TrackerBackend`/`WorkItem`/tier model. Therefore the Notion, Plane, generic
tier, spec-linking, and SDD-integration phases in the draft are removed from
this change and are follow-ups below.

The implementation is two independent chunks:

1. add the missing typed issue capability/error contract and make native and
   plugin issue providers participate in an offline caps/conformance ledger;
2. document the native and plugin tracker-provider recipes, including the
   constraints for a future CLI-backed provider.

The chunks do not add configuration, control/catalog verbs, actions, database
tables, background work, or a new event-loop path.

## Evidence from this branch

### Current seam and provider inventory

`IssueBackend` is object-safe: its async methods return `BoxFuture` and the
router stores `Box<dyn IssueBackend>` ([`crates/thegn-svc/src/issue/mod.rs:73-98`]).
The router fans out list/search, isolates account errors, dispatches by
provider-prefixed id, and accepts dynamic backends through `push_backend`
([`crates/thegn-svc/src/issue/mod.rs:225-260`, `:325-427`]). Config is an open
`[[issues.issue_accounts]]` list, with legacy single-provider synthesis
([`crates/thegn-core/src/config_issues.rs:91-153`, `:157-219`]). The factory is
the registration point ([`crates/thegn-svc/src/issue/mod.rs:148-195`]).

The live provider matrix is:

| provider      | required seam operations                                                                     | extra operations                                                                                                          | honest status today                                                                                    |
| ------------- | -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| Linear        | `list_issues`, `get_issue`, `create_issue`, `update_issue`, `search` ([`linear.rs:428-596`]) | none on the trait                                                                                                         | implemented for the five core operations                                                               |
| GitHub Issues | the same five ([`github.rs:158-320`])                                                        | `gh` is the implementation substrate, anchored to `dir` ([`github.rs:16-51`])                                             | implemented for the five core operations; no generic comment/label seam methods                        |
| Jira          | the same five ([`jira.rs:311-570`])                                                          | none on the trait                                                                                                         | implemented for the five core operations; no transitions/comments/labels/projects/subtasks on the seam |
| Kaneo         | the same five ([`kaneo.rs:482-690`])                                                         | comments, attach/detach label ([`kaneo.rs:692-766`]); public `list_projects`, `board`, `move_task` ([`kaneo.rs:413-480`]) | extra writes exist, but board/project/move are vendor-shaped and outside the seam                      |

The optional trait methods currently default to stringly
`IssueError::Api("... not supported ...")` ([`mod.rs:100-139`]) and there is
no `caps()` method. `IssueError` has no `Unsupported` variant or
`thegn_core::seam::SeamError` implementation ([`mod.rs:20-59`]), so the seam
does not yet satisfy the architecture's caps ⇔ optional-ops invariant. The
`as_kaneo()` downcast is an explicit vendor leak ([`mod.rs:141-145`, `:411-414`;
`kaneo.rs:487-491`]).

The config kind is currently `none`, `linear`, `github`, `jira`, and `kaneo`,
with no reserved tracker values ([`crates/thegn-core/src/config_issues.rs:222-231`]).
The account factory test does iterate `IssueProviderKind::ALL` and checks
`Some` versus `None`/reserved ([`crates/thegn-svc/src/conformance.rs:176-205`]),
but it does not check optional operation behavior or capabilities.

### Doctor coverage

`thegn doctor` renders the service registry's provider reports in text and JSON
([`crates/thegn-host/src/cmd/doctor.rs:307-329`]). The registry reports each
_active configured account_, not every possible tracker kind
([`crates/thegn-svc/src/seam/registry.rs:195-223`]): GitHub checks `gh`, while
Linear/Jira/Kaneo perform config checks and explicitly note that network is not
probed. Thus doctor does probe the configured native issue accounts offline,
including a missing `gh`, but does not probe unconfigured providers, and does
not report live plugin providers. It also currently emits no issue caps
([`registry.rs:205-219` constructs a bare `ProbeReport`). This is an audit
finding, not permission to make doctor perform network calls.

### Conformance coverage

The cross-seam suite checks known seam names, non-empty ids, unavailable
reasons, notes, reserved reporting, deterministic registry output, and
account-factory coverage ([`crates/thegn-svc/src/conformance.rs:32-55`,
`:139-233`]). That is good probe-shape coverage and it includes all four
current issue kinds, but it is not tracker-seam conformance: no test checks
the optional issue methods, a caps/operation agreement, `IssueError` classes,
or plugin bridge degradation.

### Plugin provider assessment

The provider plugin path is already real for the five core operations:
`PluginIssueBackend` serializes `provider.call` over a `ProviderBridge`
([`crates/thegn-svc/src/plugin/provider.rs:87-131`, `:143-230`]); the host
publishes live plugin bridges and appends them to every hydration router
([`crates/thegn-host/src/plugin_providers.rs:15-49`;
`crates/thegn-host/src/hydrate_tracker.rs:35-40`]). The plugin runtime accepts
`IssueProvider` ([`crates/thegn-svc/src/plugin/loader.rs:13-35`]), and the
runtime contract covers join/timeout behavior
([`openspec/specs/plugin-runtime/spec.md:74-86`]). A plugin can therefore
supply a tracker without touching `thegn-core` or native provider code.

The gap is capability handling: `Contribution.caps` is already a generic JSON
field ([`crates/thegn-core/src/plugin_api.rs:210-229`]), but
`PluginIssueBackend` neither reads it nor exposes `caps()`; optional calls are
forwarded and bridge errors are converted to `IssueError::Api`
([`crates/thegn-svc/src/plugin/provider.rs:164-174`, `:233-274`]). Chunk 1
parses this existing field as an issue-capability object, defaults omitted/null
to all false, refuses false-cap operations locally, and maps an upstream
`unsupported` reply to typed `Unsupported`. This is additive to the existing
wire; no `API_VERSION` bump is needed because the contribution field and
`provider.call` mechanism already exist.

## Chunk 1 design — typed caps, errors, and conformance

Add a small `issue/capabilities.rs` module rather than growing the already
large `issue/mod.rs`. Its `IssueCaps` contains only optional operations that
exist on the current issue trait: `comments` and `labels`. `IssueBackend::caps()`
is required, and its default `add_comment`, `attach_label`, and
`detach_label` implementations return `IssueError::unsupported(op)` before
any I/O. Native declarations are honest: Linear/GitHub/Jira are false for both
bits; Kaneo is true for both because its methods are implemented. The Kaneo
board/project/move API remains out of the seam for now; it is not falsely
represented as a generic capability.

`IssueError::Unsupported(&'static str)` implements `SeamError`: Unsupported,
NotConfigured, Auth, and the existing connect/timeout network cases classify
according to `thegn_core::seam`; other API/parse/subprocess cases remain
`Other` unless the existing subprocess error unambiguously identifies a missing
binary. `is_transient()` delegates to the shared trait. No provider calls are
made by the default methods or by conformance tests.

Extend `thegn_svc::conformance` with one table of the current optional
operations and a test that walks every `IssueProviderKind::ALL`. The offline
test asserts every false-cap operation takes the typed Unsupported path and
that each provider's declared cap table matches its provider-local operation
coverage tests. A deliberately overclaiming/underclaiming test double proves
the harness fails both directions. Native provider tests remain responsible for
their real I/O/argv behavior; conformance must never contact a network or
invoke a vendor binary.

For plugins, pass the parsed `IssueCaps` from the existing contribution through
the host's published provider row into `PluginIssueBackend`. Omitted/null caps
mean false. A false cap returns locally with no `provider.call`; a true cap
forwards the existing operation; an upstream `unsupported` reply still maps to
typed Unsupported as a second degradation boundary. Add bridge tests proving
local refusal, true-cap forwarding, and old manifests' all-false behavior.
Use the same caps in issue probe reports so doctor JSON exposes the selected
account's optional surface without secrets. Keep plugin-provider doctor rows
out of this chunk: standalone doctor does not run the resident plugin set; file
that as follow-up rather than pretending a live bridge exists.

No config key, env overlay, completion slot, control route/schema, help action,
or database migration is introduced. Consequently the env-overlay,
completion-slot, control-schema, and help ratchets do not change. The
forge-leak ratchet must continue to contain only the existing issue GitHub CLI
implementation; no CLI call may move into a shared helper.

## Chunk 2 design — provider authoring documentation

Add a focused `docs/extending/tracker-provider.md` and link it from the
extending index and existing provider/plugin pages. It must be self-contained:

- account-shaped registration (`IssueProviderKind`/`[[issues.issue_accounts]]`;
  no closed enum addition for an account provider), factory arm, and exact
  `IssueBackend` object-safe `BoxFuture` shape;
- `IssueCaps`, typed `Unsupported`, `SeamError` classes, offline `Probe`,
  doctor registration, provider-local tests, and the conformance ledger;
- native implementation boundaries: network/subprocess work stays off the UI
  loop, `thegn-core` remains substrate-free, credentials use secret refs, and
  a provider degrades at the edge;
- plugin registration: `IssueProvider`, `caps`, `provider.call`, account label,
  unsupported behavior, timeout, and `thegn plugin check`;
- a CLI-backed provider recipe: a concrete provider may add one implementation
  module, use explicit argv and bounded output, anchor its cwd, keep the vendor
  binary in that implementation file, and add its probe/tests/factory/config
  surfaces as needed.

The recipe explicitly records why `jira-cli` is not implemented here: Jira
already has a native REST implementation, and a selectable CLI adapter is not
actually a one-file change. It would need a selection/config contract, probe,
factory registration, output schema, auth/error mapping, and tests. A future
concrete CLI provider can follow the recipe without creating a generic
user-configured command escape hatch.

## SDD decision and follow-ups to file

BMAD-METHOD, OpenSpec, and spec-kit are documentation/skill integrations only.
The current `add-embedded-skills` design on `tg/the-20-embedded-skills` embeds
reviewed `SKILL.md` recipes and syncs them to agent directories, with no DB,
network, prompt injection, or skill execution ([`openspec/changes/add-embedded-skills/design.md:1-42`]).
That is the correct home for OpenSpec/spec-kit/BMAD guidance; THE-50 adds no
scanner, issue/spec link model, dispatch variables, or agent dependency.

File these follow-ups, each using the recipe in Chunk 2:

1. **Notion tracker provider** — define a concrete account config and property
   mapping, then implement only capabilities backed by its API.
2. **Plane tracker provider** — define workspace/project scope, pagination and
   rate-limit behavior, then implement its honest capability set.
3. **Kaneo generic-tier cleanup** — move its existing board/project/move
   operations behind a deliberately designed generic tier seam and remove
   `as_kaneo`; preserve the vendor CLI as a projection, not a seam escape.
4. **CLI-backed tracker adapter** — only if a real CLI integration is required;
   choose its config/auth/output contract first. `jira-cli` is not a shortcut
   over the existing native Jira backend.
5. **Plugin-provider doctor inventory** — make standalone doctor discover
   configured plugin manifests and report command/caps availability without
   starting resident plugins or making network calls.
6. **SDD skills** — on THE-20, add reviewed OpenSpec/spec-kit recipes and a
   BMAD recipe only after its artifact conventions are pinned. BMAD remains
   reserved as content, not a Rust config kind; Notion/Plane/Kaneo do not need
   SDD-specific code here.

These follow-ups replace the draft's proposed Notion/Plane implementations,
generic `TrackerBackend` substrate, spec-linking capability, dispatch seeding,
Jira parity expansion, GitHub label/comment expansion, Kaneo login migration,
and broad plugin wire vocabulary. Those were not re-checked into this branch's
live model and would violate the issue's audit-plus-cheap-gaps boundary.

## Validation and sequencing

Chunks 1 and 2 are file-disjoint and independent; the Lead may run them in
parallel. Chunk 1 uses only scoped crate checks and no e2e/full-workspace gate.
Chunk 2 is documentation-only and uses the same scoped consumer checks; the
final architect commit is separate and must be exactly:

`docs(the-50): architect design + chunk specs`
