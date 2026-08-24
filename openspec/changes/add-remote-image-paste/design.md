# Design

## Current paste path (what stays)

- Outer-terminal bracketed paste arrives as `InputEvent::Paste(text)`; the
  `"+` register paste reads the system clipboard via CLI tools
  (`clipboard.rs::paste()`, ms-scale, accepted on-loop as a deliberate user
  action); both feed `paste_text_into_pane` →
  `pane_writer::build_paste_bytes` (one atomic chunk, markers neutralized).
  All of this is unchanged — text paste behaves exactly as today, and text
  always wins when both text and an image are on the clipboard (the dedicated
  paste-image action covers the both-present case).

## Detection and read (never poll)

The clipboard is read **only** inside an explicit paste action. Flow for the
`"+` register: try text (existing); on empty, try image. A separate
`paste-image` action skips the text attempt. Platform candidate tables (pure,
unit-tested like the existing `candidates()`/`paste_candidates()`):

| platform | probe types             | read                                                |
| -------- | ----------------------- | --------------------------------------------------- |
| wayland  | `wl-paste --list-types` | `wl-paste -t image/png`                             |
| x11      | `xclip … -t TARGETS -o` | `xclip … -t image/png -o`                           |
| macos    | `pngpaste` if installed | `pngpaste -` (else osascript PNGf fallback)         |
| windows  | —                       | `powershell Get-Clipboard -Format Image` → temp PNG |

Absent tools degrade per candidate chain; if no image can be read the action
ends with a status message naming the missing tool. PNG is the interchange
format (every table's read yields PNG); other MIME types are converted by the
clipboard tool or skipped honestly.

Unlike the ms-scale text read, an image read plus a network transfer is NOT
acceptable on-loop: the whole flow (read → gate → write/stream → resolve
path) runs on a worker thread (QoS `Utility` — user-visible), delivering a
`PasteImageResult` over a channel **with a `TerminalWaker` pulse**; the loop
handler then pastes the path (or shows the failure) — pane input ⇒ `Panes`
damage; the status message is ordinary chrome ⇒ `Full`. No new damage
channel; no polling timeout.

## Drop targets

- **Naming**: `img-<utc-ms>-<6 rand>.png`, always generated — clipboard
  metadata (source app, suggested names) is untrusted and never used.
- **Local pane**: `$XDG_RUNTIME_DIR/thegn/paste/` (fallback
  `$XDG_STATE_HOME/thegn/paste/`), dir 0700, file 0600; paste the absolute
  path.
- **Remote pane**: resolve the pane's worktree `GitLoc`; when remote, stream
  over the existing multiplexed control channel —
  `sh_command("mkdir -p <dir> && umask 077 && cat > <dir>/<name>")` with the
  PNG bytes on stdin (the `git_with_stdin`/`stream_archive_over_ssh`
  precedent; no scp/sftp/rsync dependency, works on any target `SshTarget`
  reaches, including provider shims). Default `remote_dir =
"~/.cache/thegn/paste"` (`$XDG_RUNTIME_DIR` is unreliable over
  non-login ssh), expanded via the existing `remote_home` resolution; then
  paste the **remote** path. Transfer failure (host unreachable, disk full)
  surfaces via `model.status` — a user-invoked action never swallows its
  error.
- **Daemon-owned panes**: no daemon protocol change — the UI process performs
  the read/transfer (it owns the clipboard and the ssh control channel) and
  the path travels as ordinary pane input.

## Why not OSC 52 (pinned)

OSC 52 carries the text _selection_ to/from the outer terminal: (a) it has no
image MIME — payload is a base64 text string; (b) terminal payload caps range
from ~100 KB (xterm-lineage defaults) to 8 MiB (kitty), under base64's 4/3
inflation — screenshots routinely exceed them; (c) the _read_ direction
(query) is disabled by default in most terminals as a clipboard-exfil
hazard. It stays what it is today: the copy-direction complement
(`copymode::osc52`). The spec records this so the escape-sequence route isn't
re-attempted.

## Limits and lifecycle

- `max_image_bytes` (default 10 MiB) gates before any write or stream;
  over-limit → status message with the size and the cap, nothing sent.
- Sweep: on each image paste, entries in the _target_ drop dir older than
  `keep_hours` (default 24) are deleted — local sweep locally, remote sweep
  piggybacked on the same `sh_command` (a `find -mmin +… -delete` confined to
  the drop dir). No background timer, no reaper thread: 0% idle holds.
- No DB rows; nothing to resurrect. (`user_version` untouched.)

## Config (documented in config.toml.example)

```toml
[clipboard]
image_paste = true             # explicit action only; this gates the feature
max_image_bytes = 10485760     # refuse larger clipboard images
remote_dir = "~/.cache/thegn/paste"  # confined remote drop dir
keep_hours = 24                # age-based sweep on next paste
```

## Security

- **Explicit action only.** The clipboard is read exactly when the user
  invokes a paste; there is no watcher, no polling, no read at startup — and
  the spec pins that as a requirement (a clipboard is a cross-app secrets
  channel; silent reads are the exfil primitive).
- **Size cap bounds exfiltration.** `max_image_bytes` limits what one action
  can move off-machine; it is a hard gate before any bytes leave.
- **No content logging.** Tracing records byte counts, target kind, and
  outcomes — never image bytes, never paths joined with content.
- **Confinement both ends.** 0700 dirs / 0600 files (umask-guarded on the
  remote); generated names; the remote write is a fixed shell built from
  vetted components (dir from config expansion, name fully thegn-generated)
  streamed to the user's _own account_ on a host they already run a worktree
  on — no new trust boundary, no elevation, and the sweep's delete is
  confined to the drop dir.
- **Untrusted clipboard.** Image bytes are opaque payload: thegn never parses
  or renders them in this flow (no decoder attack surface); metadata is
  discarded.
- **Sandboxed panes**: a sandboxed local pane may not see the host paste dir;
  when the pane's worktree is sandboxed with mounts, the drop lands inside
  the worktree-visible scratch path instead — resolved at implementation via
  the sandbox mount table, noted as an open question below.

## Alternatives considered

- **OSC 52 / escape-sequence transport** — rejected (above).
- **scp/sftp** — rejected: a second transport dependency when the multiplexed
  control channel already streams stdin (host-delivery precedent).
- **Auto-upload on every copy** (mirror clipboards) — rejected outright on
  the security model: silent clipboard reads are the thing this design
  refuses to do.
- **Paste image bytes to the agent via a temp file _path_ only when the child
  requests it** (protocol sniffing) — rejected: no reliable signal; explicit
  user action is simpler and honest.

## Open questions

- Sandbox-mounted local panes: exact drop location so the child can read it
  (worktree scratch vs. a dedicated bind) — needs the mount table; spec keeps
  the requirement at "the pasted path is readable by the pane's process".
- Whether the `"+` fallback (text-empty → image) should be on by default or
  only the dedicated action — default-on chosen; revisit if muted terminals
  surprise users.
- JPEG passthrough (smaller for photos): v1 is PNG-only; a `image/jpeg`
  candidate row is additive later.
