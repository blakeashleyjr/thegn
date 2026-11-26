# Design — the Lead tool surface

## Spine pattern

New ControlApi methods take and return JSON values; typed DTOs live in the
owning core modules. All routes are `POST` with a JSON body, reads included.

## Scopes

| Capability                                                                                | Scope    | Surfaces     |
| ----------------------------------------------------------------------------------------- | -------- | ------------ |
| overview.get, notifications.list, approvals.list, automations.runs                        | Read     | http cli mcp |
| notifications.ack, automations.put, automations.rm, automations.enable, approvals.request | Write    | http cli mcp |
| approvals.resolve                                                                         | Admin    | http cli     |
| merge.drain                                                                               | MergeAdd | http cli mcp |

## Lead-authored rules

Rules carry an origin. `origin = lead` rules are refused at put time and again at
evaluation unless the action is `notify.push` or `sessions.open`. Config rules
are read-only: rm and enable answer Conflict.

## Events

`EventFrame::Notice` is delivered only when `FeedFilter.kinds` names
`notification`, `dispatch` or `automation_run`, so existing clients never see it.

## Caller attribution

`THEGN_CALLER` is sanitized to `[A-Za-z0-9._:/@-]`, at most 64 bytes, otherwise
`invalid`. It is audit-only.

Schema: v110 to v111 (automation rules), owned by the automations lane.
