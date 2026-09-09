# Design — external editor handoff

## Decisions

1. Every handoff becomes an `EditorTarget`: project, file, or file position.
2. Core performs deterministic target and argv planning. Host performs launch
   through the established editor provider/placement seam.
3. Daemon callers submit a strict, short-lived `editor.open` intent. The host
   validates containment before launch and never interpolates an untrusted shell
   command.
4. Native UI entry points call the same seam; they do not resolve editors
   independently.
5. `thegn://`, OS association, and third-party IDE extension protocols are a
   separate inbound-product decision owned by THE-104.

## Safety and runtime

- Unknown request fields are rejected.
- Relative paths resolve beneath the selected worktree; escaping targets fail.
- Slow resolution and process launch stay off the UI loop.
- GUI editors are detached/reaped; terminal editors retain ordinary pane
  placement.

## Compatibility

Existing file-open behavior remains valid. Project targets are additive, and
the public catalog exposes one operation (`editor.open`) rather than mutating
the semantically different `worktrees.open` verb.
