# THE-62 — outbound chat webhook sinks

Linear: THE-62
Title: Discord / Slack / Etc integration?
Scope: outbound notifications only

## Decision

Implement Discord incoming webhooks, Slack incoming webhooks, and a generic JSON
webhook as additional implementations of the existing `push` provider seam.
They receive notifications that thegn already routes to `push`; they do not
introduce a chat client, inbound listener, bot account, gateway connection, or
new notification source.

The issue's `serenity-rs/serenity` link is therefore a rejected alternative,
not a dependency. A Serenity-style bot would add an always-on gateway session,
async task/dependency weight, and a standing broad bot token to an AI-free
shell whose idle contract is `poll_input(None)` (the event-loop contract is
documented in `docs/ARCHITECTURE.md:54-84`). Interactive chat control, if ever
needed, must be an out-of-process client of the scoped control API/event feed,
using the existing signed command-inbox admission policy; it is not part of
this change.

The data path is:

```text
known notification producer
        -> THE-35's single NotifyState route
        -> pure core RouteDecision { push_sinks }
        -> bounded host push queue (try_send)
        -> named provider + per-sink limiter
        -> existing reqwest client, off the event loop
```

THE-35 owns the sound decision/emission seam and the live `agent_attention`
edge observer. THE-62 consumes the same `NotifyState::record` decision and
must not add a second event-bus subscriber, sound call, attention observer, or
notification row. The current branch is pre-THE-35, so implementation starts
after that branch lands/rebases; this is a deliberate serial dependency for
the overlapping `notification_route.rs`, `config.rs`, `notify.rs`, and `run.rs`
seams. The THE-35 design's one-route model is at
`/home/blake/.superzej/worktrees/thegn/tg-the-35-sound-effects/.thegn/pipeline/THE-35/architect/design.md:10-26,214-236`.

## Verified current baseline and draft corrections

The openspec draft was read as a proposal and checked against this branch.
The parts already satisfied by landed THE-12 are the object-safe push trait,
`PushKind` implemented/reserved vocabulary, an existing reqwest provider, a
bounded host queue, retrying off-loop worker, and provider registry/doctor
surface (`crates/thegn-svc/src/push/mod.rs:76-99`,
`crates/thegn-host/src/push_notify.rs:27-63`,
`crates/thegn-svc/src/seam/registry.rs:24-81`). Existing ntfy behavior remains
the compatibility baseline; ntfy/inbound command-inbox behavior is not
redesigned here.

The draft claims requiring pruning or correction are:

- It describes a future array-shaped `[[notifications.push]]`, but the branch
  currently has one `[notifications.push]` struct with nested inbox state
  (`crates/thegn-core/src/config_push.rs:42-71` and
  `crates/thegn-core/src/config.rs:4494-4566`). Use a backward-compatible
  `push.sinks` array inside the existing push table, with legacy scalar fields
  materializing one sink named by its kind when the array is empty. Do not
  break `[notifications.push.inbox]`.
- It says `RouteDecision` already carries named targets; it currently carries
  one `push: bool` and applies one global floor
  (`crates/thegn-core/src/notification_route.rs:34-53,90-156`). The core delta
  must resolve a deterministic set of sink names and apply each sink's floor
  after the final effective priority.
- It says payload shaping is fixed/no templating. That does not meet the issue
  requirement. Add a substrate-free core renderer with built-in per-kind
  message templates and explicit markdown flavors; providers only serialize
  the rendered value into their platform envelope. No user-provided template
  language is added in v1, so arbitrary formatting cannot become an execution
  surface.
- It proposes a doctor `--send-test` action. That would add a CLI value-taking
  surface and completion work for a deliberate network side effect. Cut it.
  Doctor performs an offline “dry-run ping”: validates the SecretRef, parses
  the URL, builds the request shape, and reports caps; it never POSTs. A real
  test delivery can remain a future explicit capability with its own review.
- It claims no render/model change while the issue requires a dead-letter
  counter in Monitor. Add a loop-owned, in-memory delivery snapshot and a
  conditional Notifications tab/section to the existing monitor. It has no
  DB migration, no timer, and no control/wire field. It reports runtime
  counters honestly; a fresh `thegn doctor` process cannot pretend to know a
  different host process's in-memory totals.
- Its “Discord ≈30/min” and “Slack ≈1/sec” values are operational defaults,
  not API contracts. Keep them as named provider limiter defaults, unit-test
  the state transitions, and honor a bounded `Retry-After` only within the
  existing retry budget. A full provider-specific rate-policy configuration is
  deferred.

## Core contract

### Config and names

Keep the `[notifications.push]` table and add a nested `sinks` array so the
existing ntfy table and command inbox parse unchanged:

```toml
[notifications.push]
kind = "ntfy"                    # legacy single-sink form
server = "https://ntfy.sh"
topic = "thegn-yourname"
token = "env:THEGN_NTFY_TOKEN"
min_priority = "notice"

[[notifications.push.sinks]]
name = "oncall"
kind = "slack"
url = "env:THEGN_SLACK_ONCALL_URL"
min_priority = "alert"
```

`PushConfig::sinks` is empty by default. `effective_sinks()` returns either the
explicit named entries or one legacy entry named by `kind`; names must be
unique, non-empty, and bounded. A sink has `name`, `kind`, `min_priority`, and
the provider-specific endpoint fields. `webhook`, `discord`, and `slack` use
`url`; ntfy keeps its existing `server`, `topic`, and `token` fields. There is
no new `[notifications.chat]` seam and no second routing engine.

For URL-bearing sinks, `url` is accepted only as `env:VAR` or `file:PATH`.
`SecretRef::parse` is the vocabulary, but URL validation rejects the legacy
literal variant and names only the sink (`crates/thegn-core/src/secretref.rs:77-190`).
The svc provider resolves env/file at construction/doctor-probe time; the
resolved value never enters a `Debug`, `ProbeReport`, log, error, or payload.
The existing host secret broker remains the authority for any future
keyring-capable secret family; this issue intentionally limits webhook URLs to
the two non-interactive forms required by the draft contract.

`[[notifications.rules]].route` accepts `push` (all effective sinks) and
`push:<name>` (one named sink), alongside the current inbox/desktop/toast/sound
tokens. A configured selector naming no sink is a strict config validation
error. Rule evaluation is still ordered/top-to-bottom; the last matching route
restriction and `stop` behavior remain unchanged. Unknown/custom notification
kinds continue the current conservative policy: record/display as applicable,
but do not push.

`RouteDecision` changes from a boolean to a deterministic, deduplicated list
of eligible sink names. An empty list means no push. The core applies each
sink's `min_priority` after rule priority overrides and DND have settled, so
`push:oncall` can be excluded while another sink receives the same event.
Legacy one-table config produces the same one-provider behavior as THE-12.

### Pure rendering

Add `thegn_core::notification_render` (new module; no HTTP, runtime, terminal,
filesystem, or environment dependency). It owns:

- `MarkdownFlavor` (`CommonMark`, `Discord`, `Slack`, `Plain`), selected by
  the provider kind rather than by vendor code in core;
- a `RenderedNotification` containing the stable event kind, effective
  priority, source, worktree, timestamp supplied by the caller, title, and
  rendered message;
- the built-in per-event template table, iterated from
  `NotificationKind::ALL` / `as_str()` (the single event catalog is at
  `crates/thegn-core/src/notification.rs:157-221`); and
- escaping/newline rules and visible, character-counted truncation helpers.

The template receives only notification data (`kind`, priority, message,
source, worktree, timestamp). It never interpolates config secrets. Unit tests
must cover every event template, every markdown flavor's escaping, Unicode
character bounds, stable generic fields, and the Discord visible truncation
marker. Generic JSON carries `{v, kind, priority, message, source, worktree,
ts}` with `v = 1`; the provider may use the rendered markdown/message but does
not invent a second event vocabulary.

### No core substrate leakage

The route and renderer remain pure and line-covered under the core gate. The
host/svc boundary owns queueing and I/O. This follows the crate boundary test,
which explicitly bans reqwest/tokio/HTTP substrates from core
(`crates/thegn-core/tests/crate_boundaries.rs:142-157`), and the provider seam
shape (`docs/ARCHITECTURE.md:110-149`): object-safe trait, caps, classified
errors, and `Probe`.

## Provider and delivery contract

Add focused modules under `crates/thegn-svc/src/push/`:

- `webhook.rs`, `discord.rs`, and `slack.rs` each contain pure request-shaping
  helpers plus a thin provider implementation;
- `rate_limit.rs` contains pure per-sink token-bucket/deferred-retry state;
- `PushProvider` remains object-safe (`BoxFuture`, no `async fn`), gains a
  provider `caps()` value, and keeps `probe()` synchronous; and
- the factory covers exactly the three new implemented kinds plus existing
  ntfy. Telegram/gotify/pushover remain reserved. The existing seam kind
  coverage test must continue to prove implemented == factory-supported.

The actual POST is only in provider implementation code, through the existing
`thegn_svc::provider::provider_http_client()` (`crates/thegn-svc/src/provider.rs:22-38`)
and the already-owned reqwest dependency. Do not add a crate or move HTTP into
core. “Host implementation” here means the host-wired provider worker; the
repository's architecture assigns service HTTP implementations to `thegn-svc`.

Each provider:

- builds the request from `RenderedNotification`, sets JSON content type, and
  uses only the resolved URL held privately by that provider;
- maps the effective priority to a documented provider color; Discord sends
  `content`/embed-compatible JSON and never exceeds 2,000 characters after
  the visible marker; Slack sends text plus minimal section/attachment color;
- classifies 401/403 as auth, 429/5xx/timeouts/connect failures as retryable
  (with bounded `Retry-After`), other 4xx as terminal; and
- reports provider id, caps, URL parse/SecretRef resolution state, markdown
  flavor, and dry-run request shape without revealing the endpoint.

`push_notify` becomes one bounded worker over named jobs, with a provider map
and independent limiter state per sink. `try_send` is the only producer path;
queue overflow is counted and dropped. The worker performs at most the
existing bounded attempt budget, then increments that sink's dead-letter count
and logs only sink name/status/class. It never waits on the event loop. The
worker's QoS is `Background`; no worker exists when no effective sink is
configured.

The host owns an atomic/in-memory `PushDeliverySnapshot` with, per sink,
queued, sent, retry, rate-limited-drop, queue-drop, and dead-letter totals.
`NotifyState::emit_push` fans one routed notification to the named sink list,
rendering once per provider flavor and enqueueing independently. Config reload
builds the new provider map before a short swap; it does not leak old URLs or
hold a config lock during I/O. Existing notification record writes remain
best-effort cache writes and happen before delivery enqueue.

## Monitor and doctor

Add a conditional Notifications/Delivery view to the existing Monitor modal,
fed from the loop-owned snapshot in `FrameModel`. It is visible when a named
push sink exists or any counter is nonzero, has no sampling timer, and refreshes
on the monitor's existing model/stats path. It shows sink name, kind, queue
drop, rate-limit drop, retry, sent, and dead-letter totals. The worker does not
create a new periodic wake source; a normal channel/waker pulse is used only
when the worker publishes a changed snapshot that must repaint an already-open
monitor, following the off-loop producer rule in
`docs/ARCHITECTURE.md:54-84`. No DB migration or persistent state is needed.

The existing `thegn doctor` provider registry gains one row per effective sink,
using `ProbeReport` (`crates/thegn-core/src/seam.rs:105-160`). A dry-run probe:

1. validates kind/name/priority and URL SecretRef shape;
2. resolves env/file presence and parses the URL without emitting it;
3. builds the pure request envelope and reports method/content type, caps,
   markdown flavor, and platform limit; and
4. never connects, posts, or changes the channel.

Missing optional configuration is diagnostic/degraded information, not a
doctor process failure. JSON and text use `id = sink name`; notes say
`secret: resolved|missing` and `dry-run: POST shape ready`, never the URL.
Runtime counters belong to Monitor because the standalone CLI cannot read an
interactive host's in-memory worker state without adding a new state/control
surface.

## Security, degradation, and non-goals

The existing `thegn notify push` CLI remains a durable inbox writer, like other
headless/daemon paths that do not own a `NotifyState`; it is not guessed into a
chat delivery route. Notifications emitted by the running host and its typed
background producers are the routed set. This keeps CLI behavior deterministic
and avoids making a second notification source of truth.

- Webhook URLs are bearer credentials. Reject raw URL literals in strict config
  validation and redact the resolved value from logs, diagnostics, doctor,
  errors, and debug bundles. Errors carry sink name and status only.
- Message content is intentional egress. Help must warn that branch names,
  issue titles, and log fragments may leave the machine; route by sink and
  priority deliberately.
- Rate exhaustion, queue overflow, unsupported/reserved kind, URL resolution
  failure, provider absence, network refusal, and platform format limits all
  degrade at the edge: count/report/drop or fallback without blocking inbox
  recording or the compositor.
- No Serenity, gateway, bot token, inbound commands, chat message reads,
  mentions/threading, chat TUI, ntfy redesign, DB migration, new capability
  catalog row, control API/wire field, completion slot, or shell command sink.

The “message templates per event” requirement is satisfied by the pure core
built-in template table; configurable user templates are deliberately deferred
because they would expand the config/security surface without being needed for
outbound delivery.

## Ratchets and verification

No new env-overlay key is added: nested sink fields are intentionally outside
the shallow `ConfigOverlay` contract (`crates/thegn-core/src/config.rs:5552-5615`);
run the env-overlay coverage test and preserve its ratchet. No capability
catalog row, control schema snapshot field, completion slot, keymap action, or
help action is added. The config example and existing registered
`docs/help/notifications.md` page must document every new key, so the existing
config-example/help ratchets stay green. Keep the async-trait ratchet empty and
annotate any sanctioned best-effort result exactly as the ignored-result
ratchet requires.

The openspec delta should be updated by implementation to remove the stale
`--send-test`, fixed-template, array-at-root, no-render-change, and doctor-drop
claims, and to record the Monitor counter and offline dry-run semantics. This
architect artifact is the verified design record; it does not mutate the draft
openspec files.
