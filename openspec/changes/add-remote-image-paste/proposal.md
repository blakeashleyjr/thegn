# Add image paste into panes (local clipboard → remote file drop)

Linear: THE-24

## Why

Agent CLIs running in panes take images as **file paths** — but when the pane
lives on a remote worktree (ssh/mosh placement, VPS/machine0 provider,
`[host.<name>]`), a screenshot on the local clipboard is stranded: the user
must save it, transfer it by hand, and type the remote path. thegn's paste
today is text-only end to end — `InputEvent::Paste` and the `"+` register
(`clipboard.rs::paste()`, CLI tools) feed `build_paste_bytes` (bracketed-paste
text) — so an image on the clipboard silently pastes nothing. The comparable
tool the issue cites (orca#6889) converges on the same shape thegn's existing
pieces already support: detect the clipboard image on an explicit paste,
upload it to a per-tool drop directory over the session's existing transport,
and paste the resulting path. thegn has the transport for free: every remote
worktree carries a `GitLoc`/`SshTarget` multiplexed control channel
(`sh_command` with piped stdin over ControlMaster) that can stream bytes to
the remote without scp/sftp dependencies. OSC 52 is not a viable image
channel (copy-direction, text-selection semantics, and terminal payload caps
from ~100 KB to 8 MiB before base64's 4/3 inflation) — which is precisely why
the mechanism is a file drop, not an escape sequence.

## What Changes

- **Image-aware paste on the explicit paste action** (`"+` register and a
  dedicated paste-image action): if the clipboard holds no text but holds an
  image (platform tool table: `wl-paste -t image/png`, `xclip -t image/png
-o`, macOS `pngpaste`/osascript fallback, Windows PowerShell
  `Get-Clipboard -Format Image`), thegn reads it **once, off-loop**,
  size-gates it, drops it as a generated-name file, and pastes the file's
  path into the pane wrapped in the existing bracketed-paste hardening.
- **Local pane** → file under the local runtime/state paste dir (0600 file,
  0700 dir); **remote pane** → bytes streamed over the worktree's existing
  ssh control channel into a confined remote drop dir (`~/.cache/thegn/paste/`
  by default), then the **remote** path is pasted. Both PTY owners work:
  the transfer initiates in the UI process (which owns the clipboard) and the
  path is delivered through the normal pane-input path, daemon or in-process.
- **Limits + lifecycle**: `[clipboard]` config (`image_paste`,
  `max_image_bytes` default 10 MiB, `remote_dir`, `keep_hours`); over-limit
  refuses with a status message; stale drops are swept age-based on the next
  paste (no background timer — 0% idle).
- **A new `clipboard` capability spec** (the first spec for the paste/copy
  surface) with the image-paste requirements; the new action id claims a
  `docs/help/` page (help ratchet).

## Impact

- **tasks.md**: no existing roadmap item covers this (nearest neighbors:
  J remote access; AF 399 image _preview_); the audit phase should wire it
  into group J.
- **Specs**: new capability `clipboard` (ADDED requirements only). No
  existing spec text is modified.
- **Crates**: `thegn-host` — a sibling module (`handlers/paste_image.rs` per
  the god-file guidance; `clipboard.rs` gains image read candidates, pure
  candidate tables unit-tested), reusing `GitLoc::sh_command` for the remote
  stream; `thegn-core` — pure name/dir/limit policy helpers under the 95%
  gate. `pane_writer`/`build_paste_bytes` unchanged.
- **DB**: no schema change. **Render damage**: none new (status-line messages
  are ordinary chrome `Full`; the pasted path reaches the pane as input ⇒
  `Panes`). **e2e**: generated filenames are volatile — any muse spec driving
  this must pin the name generator in `e2e_freeze`.
- **Related in-flight changes**: none overlap. THE-41 (full remote audit) is
  deferred by the user; this rides the existing `SshTarget`/`GitLoc` transport
  unchanged and adds no new connection surface. `add-viewers-and-quick-open`
  (image preview) is complementary — previewing a dropped image is its
  territory, not this change's.

## Non-goals

- Pasting image **bytes** into the PTY (no terminal protocol exists for it;
  OSC 52 analysis above pins the file-drop design).
- Reading the local clipboard when thegn itself runs on the far end of ssh
  (the user's clipboard is on their machine, unreachable by CLI tools; the
  OSC 52 _query_ path is disabled by most terminals for good security reasons
  and is not attempted) — this degrades to an honest status message.
- A general file-transfer/drop feature for arbitrary files (a natural
  follow-on; this change is clipboard-image-scoped).
- Image _rendering_ of the dropped file (the preview/graphics stack owns
  that).
