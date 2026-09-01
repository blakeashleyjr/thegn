# Add an issue tracker provider

Thegn's tracker seam is the account-shaped `IssueBackend` in
`crates/thegn-svc/src/issue/mod.rs`. It is deliberately not a closed,
generic `TrackerBackend`: configured accounts are an open list, and the
factory is the registration point. This recipe covers both a native backend
and a plugin-backed backend.

## Native provider

### 1. Register an account-shaped backend

Add the provider's account fields to the existing `IssueAccount` contract only
when they are needed by the concrete provider. A native provider normally has
an `IssueProviderKind` value in `crates/thegn-core/src/config_issues.rs`, a
factory arm in `backend_from_account`, and a selected-account probe in
`crates/thegn-svc/src/seam/registry.rs`. Do not add a new tracker tier or a
closed enum of user-defined account providers. The `[[issues.issue_accounts]]`
list is the account boundary; the legacy single-provider tables remain a
backward-compatibility path.

The object-safe seam has five required operations. Keep the exact future shape
used by `IssueBackend`:

```rust
use futures_util::future::BoxFuture;

fn list_issues<'a>(
    &'a self,
    filter: &'a IssueFilter,
) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>>;
fn get_issue<'a>(
    &'a self,
    id: &'a str,
) -> BoxFuture<'a, Result<IssueDetail, IssueError>>;
fn create_issue<'a>(
    &'a self,
    draft: &'a IssueDraft,
) -> BoxFuture<'a, Result<Issue, IssueError>>;
fn update_issue<'a>(
    &'a self,
    id: &'a str,
    patch: &'a IssuePatch,
) -> BoxFuture<'a, Result<Issue, IssueError>>;
fn search<'a>(
    &'a self,
    query: &'a str,
    limit: usize,
) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>>;
```

Implementations return `Box::pin(async move { ... })`. Do not use native
`async fn` in the trait or add a delegation enum: the router stores
`Box<dyn IssueBackend>`.

### 2. Declare only real optional capabilities

`IssueCaps` in `crates/thegn-svc/src/issue/capabilities.rs` is the optional
catalog currently shared by the seam:

```rust
IssueCaps {
    comments: true, // add_comment
    labels: true,   // attach_label and detach_label
}
```

Implement `caps()` and set a bit only when the provider implements the
corresponding operation. A false bit must degrade locally through typed
`IssueError::Unsupported("operation")`, without client, network, or
subprocess work. `Unsupported`, `NotInstalled`, and `NotConfigured` are the
seam's fall-through classes. `Auth`, `NotFound`, `RateLimited`, and
`Transient` describe a real provider response and remain final; ordinary API,
parse, and subprocess failures are `Other`. Preserve the existing
connect/timeout transient distinction and use `thegn_core::seam::SeamError`.

Do not advertise Kaneo's board, project, or move operations as generic issue
capabilities. They remain provider-shaped until a separate generic tier is
designed.

### 3. Keep boundaries safe

Network, subprocess, and other blocking work belongs on the existing
hydration/provider workers, never on the UI event loop. Keep
`thegn-core` substrate-free: provider HTTP clients and vendor processes belong
in `thegn-svc` implementation files. Credentials use the existing secret-ref
forms such as `env:NAME` or a supported `keyring:` reference; never print or
serialize resolved secrets in probe reports, errors, or logs. Compose provider
data at the seam edge and let the caller degrade there rather than leaking a
vendor client or error type into shared code.

### 4. Add an offline probe and tests

Every selected account must have a cheap, deterministic probe. The `Probe`
contract in `thegn_core::seam` is synchronous and offline: a `which`, local
configuration check, or `--version` check is appropriate; a network
round-trip is not. Register the selected issue accounts in
`crates/thegn-svc/src/seam/registry.rs::issue_probes`, include their
`IssueCaps` in the `ProbeReport`, and retain the account name in the report
id. `thegn doctor` reports configured accounts, not every possible provider
and not resident plugin providers.

Add provider-local tests for each required operation and each positive
optional operation. Keep I/O and vendor argv assertions in the provider's own
module. Extend the offline ledger in
`crates/thegn-svc/src/conformance.rs` for every `IssueProviderKind::ALL`
entry: false-cap defaults must return typed `Unsupported`, while declared
positive bits must match real methods. The conformance tests must not contact a
network or invoke a vendor binary. Preserve probe shape, reserved-kind,
determinism, and factory coverage.

## Plugin provider

A plugin registers the `IssueProvider` extension point. Its contribution
`label` is the account label shown in the issue panel, and its existing `caps`
object declares the optional issue surface:

```toml
capabilities = ["surface:provider"]

[[contributions]]
id = "tracker"
extension_point = "IssueProvider"
label = "Team tracker"

[contributions.caps]
comments = true
labels = false
```

The exact manifest still needs the normal plugin fields (`id`, `name`,
`version`, `api`, `command`, and mode/timeout as appropriate). Omitted or
`null` caps mean `{ comments = false, labels = false }`; unknown cap fields are
rejected, and malformed declarations are treated as all-false by the host.
When a bit is false, the host returns typed `Unsupported` locally and does not
send an RPC. When it is true, the host sends the existing newline-delimited
JSON request:

```json
{
  "id": 1000000,
  "method": "provider.call",
  "params": { "seam": "issues", "op": "list_issues", "args": {} }
}
```

Use the issue seam's JSON argument and result shapes for `list_issues`,
`get_issue`, `create_issue`, `update_issue`, and `search`. Optional operations
use `add_comment`, `attach_label`, and `detach_label`. An upstream
`{"code":"unsupported"}` response is also mapped to typed `Unsupported`,
which gives the wire a second degradation boundary. Calls run through the
resident bridge and honor the plugin's `timeout_secs`; a timeout or dead
process becomes a provider error rather than hanging the UI.

Check the manifest and negotiated extension point with:

```sh
thegn plugin list
thegn plugin check
```

The standalone doctor does not start resident plugins or inventory live plugin
providers. That inventory is a separate follow-up; the plugin bridge is
published to hydration workers when a resident provider is running.

## CLI-backed provider recipe

If a concrete tracker needs a vendor CLI, keep it as one provider
implementation module under `crates/thegn-svc/src/issue/`:

1. Build an explicit argv vector. Never interpolate config into a shell
   command or add a generic user-configured command escape hatch.
2. Anchor the command's working directory to the relevant worktree or
   repository, as GitHub's `gh` implementation does.
3. Bound output size and execution time, parse a documented machine-readable
   format, and map authentication, missing-binary, transient, and parse
   failures to the typed seam errors.
4. Add the offline probe, factory/config registration, provider-local parser
   and argv tests, and the conformance entry required by the selected config
   surface.

Keep the vendor binary call in that implementation file so the forge/CLI leak
ratchets can enforce the boundary. A future adapter can follow this recipe
without changing the common issue seam or adding a generic command setting.

`jira-cli` is intentionally not implemented in this issue. Jira already has a
native REST backend, and a selectable CLI adapter would require a selection and
configuration contract, probe, factory registration, output schema, auth and
error mapping, and tests. It is not a one-file shortcut over the native Jira
provider.

## Follow-ups

Notion and Plane need concrete account/property and pagination/rate-limit
contracts first. Kaneo's board/project/move operations need a deliberately
generic tier before `as_kaneo` can go away. Plugin-provider doctor inventory,
and any OpenSpec, spec-kit, or BMAD skill integration, are separate follow-ups;
they do not add tracker code here.
