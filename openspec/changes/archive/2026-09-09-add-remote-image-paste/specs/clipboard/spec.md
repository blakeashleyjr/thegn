# Clipboard

## ADDED Requirements

### Requirement: Pasting a clipboard image drops a file and pastes its path

When an explicit paste action finds an image (and no text) on the system
clipboard, thegn SHALL read the image once via the platform clipboard tools
(PNG interchange), write it to a drop location as a generated-name file, and
paste the file's absolute path into the focused pane through the existing
bracketed-paste hardening. For a pane whose worktree is **local**, the drop
location is thegn's local paste directory; for a pane whose worktree is
**remote**, the bytes SHALL stream over the worktree's existing ssh control
channel (multiplexed `SshTarget`/`GitLoc` transport — no scp/sftp dependency)
into a confined remote drop directory, and the **remote** path is pasted.
Filenames MUST be thegn-generated (clipboard-supplied metadata is never
used); the whole read/transfer flow MUST run off the event loop with the
result delivered over a channel that pulses the `TerminalWaker`. Escape-
sequence transport (OSC 52) MUST NOT be used to carry image data — it is
text-selection-only, size-capped by terminals, and its read direction is a
security hazard; the file drop is the mechanism. Text paste behavior is
unchanged, and text wins when both are present (a dedicated paste-image
action covers that case). A transfer or read failure SHALL surface as a
status message naming the cause — never silently dropped.

#### Scenario: Screenshot into a remote agent pane

- **WHEN** the user copies a screenshot and invokes paste in a pane whose
  worktree lives on an ssh-reached host
- **THEN** the image streams over the worktree's existing control channel to
  a generated-name file in the remote drop directory, and the remote absolute
  path is pasted into the pane as one bracketed-paste chunk

#### Scenario: Local pane pastes a local path

- **WHEN** the user pastes a clipboard image into a local-worktree pane
- **THEN** the image lands in the local paste directory and its absolute
  path (readable by the pane's process) is pasted

#### Scenario: No image and no text is an honest no-op

- **WHEN** paste is invoked with neither text nor a readable image on the
  clipboard (or the platform clipboard tool is missing)
- **THEN** nothing is written or sent, and a status message names what was
  missing

### Requirement: Image paste is explicit, size-capped, and never logged

thegn SHALL read the clipboard only within an explicit user paste action —
never on a timer, at startup, on focus, or from any background watcher — and
SHALL enforce `[clipboard] max_image_bytes` (default 10 MiB) **before** any
byte is written locally or leaves the machine, refusing over-limit images
with a status message stating the size and the cap. Image content MUST never
be logged or traced (byte counts and outcomes only), and `[clipboard]
image_paste = false` SHALL disable the feature entirely.

#### Scenario: Over-limit image is refused before transfer

- **WHEN** the clipboard image exceeds `max_image_bytes` at paste time
- **THEN** no file is created, nothing is streamed to any host, and the
  status line reports the image size and the configured cap

#### Scenario: No paste action, no clipboard read

- **WHEN** thegn runs without the user invoking a paste action
- **THEN** thegn never executes a clipboard-read tool

### Requirement: Dropped images are confined and reaped

Drop directories SHALL be created mode 0700 with files written 0600
(umask-guarded on the remote side), locally under thegn's runtime/state paste
directory and remotely under `[clipboard] remote_dir` (default
`~/.cache/thegn/paste`) in the target user's own account — never outside the
drop directory. On each image paste, files in the target drop directory older
than `[clipboard] keep_hours` SHALL be swept (delete confined to the drop
directory), with no background timer or reaper thread — the 0%-idle contract
is untouched.

#### Scenario: Stale drops are swept on the next paste

- **WHEN** an image paste targets a drop directory containing files older
  than `keep_hours`
- **THEN** those files are deleted as part of the same operation, and no
  path outside the drop directory is touched
