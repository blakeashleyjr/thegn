# Calendar command plugin protocol

Calendar command accounts receive their query in environment variables and
write newline-delimited JSON to stdout. See [calendar help](../help/calendar.md)
for the complete event shapes and an example.

An `events` response may include `mode`:

| `mode`      | Meaning                                                                                                                  |
| ----------- | ------------------------------------------------------------------------------------------------------------------------ |
| `snapshot`  | The complete requested window; cached events absent from the response are removed. A `sync_token` may still be returned. |
| `delta`     | Changes since the supplied `THEGN_CAL_SYNC_TOKEN`; valid only when a token was supplied.                                 |
| `unchanged` | No changes; valid only with a supplied token and an empty event/deletion list.                                           |
| `reset`     | Replace the cache, including when the response is empty.                                                                 |

Omitting `mode` keeps the legacy inference: without a request token the response
is a snapshot; with a token, a returned `sync_token` means delta and no returned
token means snapshot. New plugins should declare `mode` so a snapshot can safely
return a cursor and a delta can omit its next cursor without changing meaning.
