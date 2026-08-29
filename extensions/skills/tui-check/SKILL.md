---
name: tui-check
description: Verify a thegn change by actually driving the built binary — open it in a PTY with muse, send chords, read the screen as text or a PNG, wait for UI text, and turn the check into an e2e spec. Use after changing anything that renders (chrome, panel, sidebar, keymap, input) or when asked to reproduce a UI bug.
---

# Check a thegn change for real (`/tui-check`)

The reference is `docs/testing-with-muse.md` (environment knobs, spec
anatomy, the traps, reading artifacts); this is the short form.

Unit tests can't see a frame. `muse` (on the dev-shell PATH) can: it runs the
built `thegn` in a real PTY and lets you look, act, and look again. Every
step below is a plain shell command.

## Setup (once per check)

When the executable is stale, build only the host crate, then open thegn in an
isolated, deterministic environment — the same one `just e2e` uses. Keep this
session alive while iterating so each look → act → look cycle reuses it:

```bash
cargo build -p thegn-host
T=$(mktemp -d); mkdir -p "$T/home" "$T/cfg/thegn" "$T/state" "$T/run" "$T/bin"
printf '[sandbox]\nbackend = "none"\n[media]\nenabled = false\n' > "$T/cfg/thegn/config.toml"
printf '[user]\nname = muse\nemail = muse@example.invalid\n' > "$T/gitconfig"
printf '#!/bin/sh\nexport PS1="$ " PROMPT_COMMAND=\nexec /bin/sh --norc --noprofile -i\n' > "$T/bin/e2esh"; chmod +x "$T/bin/e2esh"
export MUSE_SOCKET="$T/muse.sock"
muse session open --name tg --size 120x40 --cwd "$PWD" \
  --env HOME="$T/home" --env XDG_CONFIG_HOME="$T/cfg" --env XDG_STATE_HOME="$T/state" \
  --env XDG_RUNTIME_DIR="$T/run" --env SHELL="$T/bin/e2esh" \
  --env GIT_CONFIG_GLOBAL="$T/gitconfig" --env GIT_CONFIG_SYSTEM=/dev/null \
  --env DBUS_SESSION_BUS_ADDRESS="unix:path=/dev/null/e2e-no-dbus" \
  --env THEGN_E2E=1 --env MUSE_READY=1 --env THEGN_NO_DAEMON=1 --env THEGN_SKIP_ONBOARDING=1 \
  --env THEGN_LOG=debug --env TERM=xterm-256color -- "$PWD/target/debug/thegn"
muse session wait tg --visible NORMAL --timeout-ms 20000
```

`THEGN_E2E=1` freezes stats/clock/version/activity so frames are comparable;
`THEGN_NO_DAEMON=1` keeps panes in-process (drop it to test the daemon route).

## Look → act → look

```bash
muse session snap tg                              # the settled screen, as text
muse session send tg --key ctrl+alt+p             # a host chord
muse session send tg --text "echo hi" --key enter # pane input
muse session wait tg --visible "2 commits" --timeout-ms 8000
muse session snap tg --kind pixel --out "$T/shot.png"   # view the PNG
muse session resize tg 80x24                      # breakpoints
muse session screen tg                            # cursor/title/modes (JSON)
```

`wait` exits 1 (with the reason) when the text doesn't show: read the next
`snap` to see what did. Chords: `ctrl+alt+p`, `alt+t`, `shift+tab`, `f1`,
`escape`, `ctrl+space`. Mouse: `--mouse '@row,col'`.

## When it looks wrong

- `muse session logs tg` — every byte thegn wrote (escape sequences included).
- `thegn logs tail -n 50 --path "$T/state/thegn/logs/thegn.log"` — its log
  (`THEGN_LOG=debug` above; `thegn::frame=debug` for render decisions).
- `muse session trace tg --out "$T/trace"` then `muse trace "$T/trace" --frame N`
  — every stable frame, for "when did it go wrong".

## Turn it into a test

```bash
muse session export-spec tg --out test/muse/specs/NN-feature.yaml
```

Then make it look like its neighbours: use the shared fixture `spawn:` block
(pinned commit dates), the startup waits, and the closing `check_file` guard;
add `snapshot:` steps for layout changes. Once the UI change is settled, run
the full `just e2e` suite as the final intentional validation; if snapshots
change, use `just e2e-update`, review the baseline diff, and run `just e2e`
again. Do not run either full suite after every small edit.

## Always

- `muse session close --all` when done (a forgotten session is a live thegn).
- Never point a session at your real `$XDG_STATE_HOME` with the daemon on:
  `THEGN_NO_DAEMON=1` launches against a real state dir stop persisted
  daemon sessions.
