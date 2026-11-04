# Resident plugin app tabs (API 0.4)

A resident plugin can contribute an `AppTab` with a declared `surface` and the
`surface:app` capability. The host creates a top-level tab named
`plugin:<plugin-id>:<contribution-id>`. Configure that id in `[apps]
default_tab` or `tab_order`. App tabs are experimental and available on the dev
channel only (`THEGN_CHANNEL=dev`; see [release channels](../help/release-channels.md)).

The host owns widgets, layout, rendering, keyboard routing, and recovery UI.
Plugins own domain logic, network access, credentials, durable storage, and
background work. API 0.3 plugins continue to load with their existing behavior.

## Updates and views

A resident plugin sends the `update` notification with `params.surface` naming
its declared surface and `params.app` holding an `AppView`:

```json
{
  "method": "update",
  "params": {
    "surface": "acme/workspace",
    "app": {
      "state_scope": "account-1",
      "revision": 1,
      "root": {
        "kind": "text",
        "id": "summary",
        "title": "Workspace",
        "text": "Ready"
      },
      "status": ""
    }
  }
}
```

`AppView` carries `state_scope`, a monotonically increasing `revision`, a root
widget tree, and optional status text. Stable widget, row, field, and form
instance ids preserve local focus, selection, and edits. Forms retain raw field
strings so temporarily invalid input remains editable. Actions and forms carry
an opaque binding with captured target ids and optional preconditions; echo it
back unchanged when submitting.

An `update` may also include `focus: true` and `focus_ticket`. The host accepts
the focus request only once, for the originating session and surface, while the
ticket is unexpired, the user has not navigated, and no host modal owns input.
Ordinary background updates cannot take focus.

The validator limits the widget tree to depth 12 and 2048 total widgets,
collections to 4096 rows, forms to 64 fields, and ids to 128 bytes. Sparse grids
are at most 512 by 256 with 32768 cells. Control characters, overlaps,
invalid dimensions, and overflowing wide glyphs are rejected. JSON nesting is
limited to 32 levels and a transport frame to one MiB. Invalid updates leave
the last accepted view in place.

## Events and reliability

The host sends the plugin `on_event` with
`{"kind":"App","payload": AppEventParams}`. The payload identifies the
surface, session, state scope, and view revision and contains a tagged event.
Reliable events (actions, activation, submissions, cancellation, sorting, and
explicit invocations) are sent as requests with an RPC id; reply through the normal
`result` or `error` envelope. A successful result means local acceptance is
complete and any durable edit has been committed. Notifications such as focus,
visibility, selection, draft changes, viewport changes, and ordinary keys are
sent without an RPC id and may be coalesced. A reliable event is a coalescing
barrier.

`activate` includes the current `session` token, the granted capabilities, and
`background`, which is true only when the plugin declared background syncing,
was granted `background:sync`, and the plugin declares `background = true`. A plugin
must reject events from a different session. A timeout or process loss after a
reliable request was admitted leaves an unknown outcome; the host retains the
submitted draft and never retries a mutation automatically. F2 opens draft
recovery (`plugin-app-drafts`); F3 opens unknown-outcome request recovery
(`plugin-app-requests`).

A plugin can request focus from a user-initiated callback. The host issues a
one-use ticket tied to that session and surface, valid for five seconds. Any
subsequent user key, paste, or mouse input invalidates the ticket, including
navigation within Work. A visible host overlay denies focus.

## Input and background work

Inside a plugin app, the configured quit, detach, quit-kill, palette, and help
chords remain available, as do the Alt tab-switch chords. Other keys and paste
go to the app unless a visible host overlay owns input. F2 and F3 are rebindable
and pass through to terminal programs in Work.

A hidden app stops scheduled work by default. `background = true` together with
the `background:sync` capability permits hidden synchronization and requires
`trusted_native = true`; non-surface capabilities are subject to the existing
native trust gate. This is a trust decision, not an operating-system sandbox.

The preview CLI, SDK, and example plugins are planned separately under
THE-1674, THE-1675, and THE-1676.
