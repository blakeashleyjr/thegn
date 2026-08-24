# Testing thegn with muse — for developers and agents

[muse](https://github.com/blakeashleyjr/muse) is Playwright for terminals: it
runs a program in a real PTY, keeps a faithful screen model, drives it
(keys / mouse / paste / resize), asserts on what's on screen with retrying
web-first checks, and snapshots frames. thegn uses it two ways:

1. **The gate** — `just e2e` runs `test/muse/specs/*.yaml` against the built
   binary and diffs snapshots against `test/muse/snapshots/` (`just ci-local`;
   opt-in in CI with `[ci-e2e]` until the baselines are re-recorded).
2. **The loop** — `muse session` keeps a thegn alive between commands so a
   developer or an agent can look, act, and look again while iterating.

Both use the same binary, the same isolation, and the same determinism
freeze, so what you see by hand is what the gate sees.

Contents: [Quick start](#quick-start-the-loop) · [Environment](#the-environment-and-why-each-knob-exists) ·
[Reading the screen](#reading-the-screen) · [Writing a spec](#writing-a-spec) ·
[Things that bite](#things-that-bite) · [Artifacts](#when-a-case-fails-reading-the-artifacts) ·
[Baselines](#snapshot-baselines) · [Agents / MCP](#for-agents-mcp-and-the-skill) ·
[macOS](#macos) · [Internals](#how-thegn-cooperates)

## Quick start (the loop)

```bash
just build                                   # target/debug/thegn (+ fake_lsp)
T=$(mktemp -d); mkdir -p "$T/home" "$T/cfg/thegn" "$T/state" "$T/run" "$T/bin"
printf '[sandbox]\nbackend = "none"\n[media]\nenabled = false\n' > "$T/cfg/thegn/config.toml"
printf '#!/bin/sh\nexport PS1="$ " PROMPT_COMMAND=\nexec /bin/sh --norc --noprofile -i\n' > "$T/bin/e2esh"
chmod +x "$T/bin/e2esh"
export MUSE_SOCKET="$T/muse.sock"            # a private daemon for this session

muse session open --name tg --size 120x40 --cwd "$PWD" \
  --env HOME="$T/home" --env XDG_CONFIG_HOME="$T/cfg" --env XDG_STATE_HOME="$T/state" \
  --env XDG_RUNTIME_DIR="$T/run" --env SHELL="$T/bin/e2esh" \
  --env THEGN_E2E=1 --env MUSE_READY=1 --env THEGN_NO_DAEMON=1 --env THEGN_SKIP_ONBOARDING=1 \
  --env THEGN_LOG=debug --env TERM=xterm-256color -- "$PWD/target/debug/thegn"

muse session wait tg --visible NORMAL --timeout-ms 20000   # first frame
muse session snap tg                                       # the screen, as text
muse session send tg --key ctrl+alt+p                      # a host chord
muse session send tg --text "echo hi" --key enter          # pane input
muse session wait tg --visible "hi" --timeout-ms 5000
muse session snap tg --kind pixel --out "$T/shot.png"      # a PNG (open it)
muse session close tg                                      # always
```

`wait` exits 0 when the condition holds, 1 when it doesn't (with the reason),
2 when muse itself failed. `snap` waits for the screen to settle; `screen`
dumps it right now with cursor/title/modes as JSON. Every verb takes `--json`.

Other verbs: `resize tg 80x24`, `send --paste`, `send --bytes '\e[A'`,
`send --mouse '@row,col'` (or `release:left@r,c`, `wheel_down@r,c`),
`logs tg` (every byte thegn wrote), `trace tg --out DIR` (casts + every
stable frame), `list`, `close --all`, `export-spec tg --out file.yaml`.
Chords are `ctrl+alt+p`, `alt+t`, `shift+tab`, `f1`, `escape`, `ctrl+space`.

## The environment, and why each knob exists

`just e2e` builds this environment in `_e2e_env` (justfile); the quick start
above is the same thing by hand. Every piece is there because its absence
made frames differ between runs or machines:

| Knob                                                                                          | Why                                                                                                                                                                                                                                                                                                |
| --------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| throwaway `HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, `GIT_CONFIG_GLOBAL` | never read your config, never touch your DB, never find your daemon socket                                                                                                                                                                                                                         |
| `THEGN_E2E=1`                                                                                 | the determinism freeze (`crates/thegn-host/src/e2e_freeze.rs`): fixed stats, clock `Thu Jan 1 · 12:00`, version `v0.0.0-e2e`, activity dots never decay, media badge off                                                                                                                           |
| `MUSE_READY=1`                                                                                | skips the dormant launch splash and makes thegn emit `OSC 5379;muse:ready` after each flushed frame, so muse knows a frame is complete without guessing from timing                                                                                                                                |
| `THEGN_NO_DAEMON=1`                                                                           | panes run in-process. The daemon route would leave a detached daemon + shell behind per run. Drop it only to test that route (see `31-daemon-panes.yaml`, which cleans up after itself) — and **never** against your real state dir: a no-daemon launch claims and stops persisted daemon sessions |
| `THEGN_SKIP_ONBOARDING=1`                                                                     | no first-run wizard eating the keystrokes                                                                                                                                                                                                                                                          |
| `SHELL=…/e2esh` (fixed `$ ` prompt)                                                           | the pane's prompt, title and the sidebar row label would otherwise carry `user@host:cwd`. thegn's pane env allowlist drops `ENV`/`PS1`, and NixOS bash sources `/etc/bashrc`, hence a wrapper                                                                                                      |
| `[sandbox] backend = "none"`, `THEGN_SANDBOX_BACKEND=none`, `[media] enabled = false`         | no container bring-up; the media watcher reaches the player even with the session bus cut                                                                                                                                                                                                          |
| `DBUS_SESSION_BUS_ADDRESS=unix:path=/dev/null/…`                                              | belt and braces for the above                                                                                                                                                                                                                                                                      |
| fixture repo with `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` pinned                               | commit hashes appear in the git panel; pinned dates make them identical everywhere                                                                                                                                                                                                                 |
| `THEGN_LOG=warn` (specs) / `debug` (by hand)                                                  | the log file exists from the start, so the spec's log guard can read it; panics are routed into it                                                                                                                                                                                                 |

## Reading the screen

- `snap` = text of the settled frame, trailing blanks trimmed. Assert on
  **stable UI text**: the statusbar mode (`NORMAL`, `EMACS`, `VIM NORMAL`,
  `⌁ LOCKED`), section headers (`1 changes`, `2 commits`, `3 branches`),
  panel tabs (`git  work  system`), tab chips (`home   1   2`), pane frames
  (`e2esh · home`), zoom badges (`MAX`, `ZOOM`), keyhints (`Ctrl g lock`,
  `open / fold`, `checkout`), status messages (`Theme: storm`,
  `Root workspace cannot be deleted`).
- Pane content: the shell prompt is `│$ ` (border + prompt). Anchor on it to
  know a pane is up; anchor on `│<text>` to find output at a row start.
- `snap --kind styled` shows attributes/colors — the only way to tell themes
  apart (spec 14 uses styled snapshots).
- `screen` tells you whether thegn is on the alt screen, where the cursor is,
  and which modes are on (mouse, bracketed paste, sync output, kitty flags).

## Writing a spec

Specs are YAML under `test/muse/specs/`, numbered; `muse run` expands the
`matrix` (profiles × sizes) into cases. Anatomy of one (copy the shared
parts from a neighbour):

```yaml
name: panel_git # the case id: name [profile WxH]
# What this proves, in a sentence or two.
case_tmp_env: XDG_STATE_HOME # a fresh dir per case, exported as this var
matrix:
  profiles: [xterm] # xterm | vt220 | kitty | screen | dumb
  sizes: ["100x30", "160x40"]
spawn: # the shared fixture block (pinned dates), then exec thegn
  - sh
  - -c
  - |
    set -e
    repo="$XDG_STATE_HOME/repo"; mkdir -p "$repo"; cd "$repo"
    git init -q -b main
    git config user.email e2e@example.invalid; git config user.name e2e
    export GIT_AUTHOR_DATE=2020-01-01T00:00:00Z GIT_COMMITTER_DATE=2020-01-01T00:00:00Z
    printf 'hello\n' > README.md; git add -A; git commit -q -m init
    exec thegn
env:
  XDG_STATE_HOME: "{case_tmp}" # {case_tmp} expands to the per-case dir
  THEGN_LOG: warn
  MUSE_READY: "1"
  TERM: xterm-256color
sync:
  quiet_window_ms: 2000 # "stable" = this long with no output (or the ready marker)
  max_settle_ms: 10000
snapshot_defaults: # only if the spec takes snapshots
  kind: text
  masks:
    - { content: "(?m)^  (NORMAL|EMACS|VIM NORMAL) +\\?.*$" } # statusbar tail: TTL messages
  normalize:
    - { re: "\\b[0-9]+(s|m|h|d|w|mo|y)\\b", replace: "<age>" } # relative ages drift
steps:
  - expect_visible: { text: "NORMAL", timeout_ms: 20000 } # startup
  - expect_visible: { regex: "│\\$ ", timeout_ms: 20000 }
  - expect_visible: { text: "working tree clean", timeout_ms: 15000 } # git hydration landed
  - key: { key: "right", mods: [ctrl] } # FOCUS the panel
  - sleep_ms: 30
  - key: { key: "3" } # jump to a section header…
  - key: { key: "enter" } # …and open it
  - expect_visible: { regex: "\\* main", timeout_ms: 5000 }
  - snapshot: { name: branches }
  - check_file: # always last: the log guard
      path: "{case_tmp}/thegn/logs/thegn.log"
      reject_re: "ERROR|thread '.*' panicked|WARN.*OOM|WARN.*corrupt|subtract with overflow|index out of bounds"
```

Steps: `write`, `write_line`, `paste`, `key {key, mods}`, `mouse {row, col,
button, action, mods}`, `resize "WxH"`, `sleep_ms`, `expect_visible`,
`expect_not_visible`, `expect_text {…, equals}`, `expect_contains`,
`expect_count {…, eq|min|max}`, `expect_style {…, bold|fg|bg…}`,
`expect_exit {code}`, `snapshot {name, kind, masks, normalize}`,
`check_file`, `watch_log`. Locators: `text`, `regex` (line-anchored `^`/`$`
work), `line: N`, `cell: [r,c]`, `region: [r,c,w,h]`, `cursor`; each takes
`timeout_ms`. Key names: letters, `enter`, `tab`, `escape`, `space`,
arrows, `home`/`end`, `pageup`/`pagedown`, `f1`…; mods `ctrl`, `alt`,
`shift`, `super`.

Prefer an `expect_*` over a `sleep_ms` whenever there is an anchor; a sleep
is for "let hydration land before a snapshot" and the deliberate gaps below.

The fastest way to a correct spec: drive the scenario by hand with
`muse session`, then `muse session export-spec tg --out test/muse/specs/NN-x.yaml`
and tidy it into the shape above (the export records your inputs and every
`wait` that held as the matching `expect_*`).

## Things that bite

Learned the hard way while rewriting the suite; each cost a run.

- **Keys go to the shell unless the panel/sidebar is focused.** Opening the
  panel (`Ctrl-Alt-p`) doesn't focus it; `Ctrl-Right` does (and opens it if
  closed). `Ctrl-Left`/`Escape` return to the pane; `Alt-s` focuses the
  sidebar. A spec that sends `j`/`k`/`1`/`2` after merely opening the panel
  is typing into bash.
- **Panel sections:** a digit jumps to a section _header_, `Enter` opens it;
  `j`/`k` walk rows once one is open. Opening a one-commit Commits section
  drills straight into its file list. `Tab`/`Shift-Tab` cycle the panel's
  tabs (git → work → notifications → system) — not `]`/`[`.
- **The sidebar toggle is a three-state cycle:** full → rail (repo initial
  only) → hidden → full. Assert on `WORKSPACES` (the header), not on the
  word "sidebar", which is also a keyhint.
- **Tab chips that don't fit are dropped silently** — with the panel open at
  100 columns the strip is ~22 columns. The tab exists; close the panel
  before asserting chip counts. (KNOWN_ISSUES; tasks.md 745.)
- **Keys typed into a brand-new tab before its shell is up are dropped** (by
  design — there's no pane yet). Wait for `│\$ ` after `Alt-t`.
- **Give a bare Escape 120 ms.** `ESC` glued to the next chord's bytes in one
  read is `Alt+…`. Escape-heavy specs run under the `kitty` profile, where
  Escape is `CSI 27 u` and can't alias. Space chords by ~30 ms too — a
  dozen chords in one write can straddle a read under load.
- **Transient status messages have a TTL** (`Theme: storm`, `branch log
renders…`). Assert on them with `expect_visible` right after the action;
  never let them into a snapshot unmasked (the default mask above).
- **Hydration is async.** The panel's `2 commits <hash> init` lands after
  `working tree clean`; wait for what you're about to use.
- **Breakpoints hide chrome.** Below ~80 columns the panel auto-hides; at 40
  the sidebar goes. Pin the state explicitly after a resize storm
  (`Ctrl-Right` then `Ctrl-Left`) rather than relying on the reveal
  (tasks.md 748).
- **New volatile chrome must be pinned in `e2e_freeze.rs`** — anything that
  changes on its own (a clock, a counter, a spinner) flaps every snapshot
  that contains it.

## When a case fails: reading the artifacts

`just e2e` keeps `e2e-results/<name>__<profile>__<WxH>/` for every failing
case (CI uploads the directory as the `e2e-results` artifact):

| File                                                            | What it is                                                                                                        |
| --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `final.txt` / `final.png`                                       | the screen when the case ended — read this first                                                                  |
| `final.json`                                                    | cursor, title, modes, alt-screen, generation                                                                      |
| `result.json`                                                   | every assertion with `ok` and `detail`; find the first `false`                                                    |
| `<snap>.actual.txt` / `.diff.txt` / `.baseline.txt` (or `.png`) | a snapshot mismatch, three ways                                                                                   |
| `trace/frames.jsonl`                                            | every stable frame; `muse trace trace --frame N` renders one                                                      |
| `trace/input.cast`, `trace/output.cast`                         | what was sent and what thegn wrote, with timestamps (asciinema v2)                                                |
| `trace/steps.jsonl`                                             | the `begin_step` boundaries with their assertions                                                                 |
| `<case_tmp>/thegn/logs/thegn.log`                               | thegn's own log — inside the case's state dir while it exists; raise `THEGN_LOG` in the spec's `env` to keep more |

Typical diagnosis: `result.json` → which assertion; `final.txt` → what was
actually on screen; `input.cast` → were the keys sent when you thought (a
burst in one millisecond is a spec bug, see above); `trace --frame` → when
the state diverged.

## Snapshot baselines

`test/muse/snapshots/<spec>__<name>/<profile>__<WxH>__<os>.txt` (styled
snapshots get `#styled` in the directory name). `just e2e` runs with `--ci`:
a missing baseline is a **failure**, never an auto-create. After an
intentional UI change run `just e2e-update`, then review the diff under
`test/muse/snapshots/` like code — it is the rendered consequence of your
change. `just e2e` must then pass twice.

Masks and normalizers in a spec's `snapshot_defaults` apply to every
snapshot in it; per-step `masks:`/`normalize:` add to them. A `content` mask
blanks whatever the regex matches; a `rect` mask blanks an area.

## For agents: MCP and the skill

- **Skill:** `extensions/skills/tui-check/SKILL.md` is the Claude Code
  recipe for "verify this change for real" — the quick start above plus the
  debugging ladder and how to promote a session to a spec. Point `/tui-check`
  at it.
- **MCP:** `claude mcp add muse -- muse mcp` registers the session verbs as
  tools (`open`, `send`, `resize`, `snap` — pixel snaps come back as an
  image block — `screen`, `wait`, `logs`, `list`, `close`, `export_spec`,
  `run_spec`). `wait` returns `ok: false` rather than erroring when the text
  isn't there, so the agent reads the reason instead of retrying blind.
- **From inside a daemon-backed pane** (the default route outside tests)
  thegn exports `THEGN_SESSION_ID` and `THEGN_CONTROL_SOCKET`, so a program
  in a pane can read its own screen with
  `thegn session snapshot --session "$THEGN_SESSION_ID" --text` and see its
  siblings with `thegn session list --json`.
- Rules: always `close` what you opened (a forgotten session is a live
  thegn); isolate with `MUSE_SOCKET`; never drive your own live instance.

## macOS

Untested at the time of writing; expected first-contact items:

- Baselines are `__linux`-suffixed. The first `just e2e` on a Mac fails with
  "missing baseline"; run `just e2e-update` to record `__macos` ones and
  commit them alongside (muse's font and renderer are deterministic, so text
  baselines should match byte-for-byte — a diff there is a real difference).
- `31-daemon-panes.yaml` scopes its cleanup by reading `/proc/<pid>/environ`;
  it needs a darwin variant (or a skip) before the suite is green there.
- The pane shell wrapper uses `/bin/sh --norc --noprofile -i`; mac's
  `/bin/sh` is bash 3.2 and accepts it.
- The interactive loop (`muse session`) needs none of the above.

## How thegn cooperates

- `MUSE_READY` → `emit_muse_ready_marker` (`frame_write.rs`) writes
  `OSC 5379;muse:ready` through the frame writer after a flush when no input
  is pending, so muse declares a frame stable immediately instead of waiting
  out `quiet_window_ms`. It cannot express "hydration finished" — wait for
  content.
- `THEGN_E2E` → `e2e_freeze.rs` (stats seeded from the first frame, clock,
  version, activity FSM not polled, media forced off in every config load).
- Panics → `log_trace::install_panic_hook` re-emits them through `tracing`
  so the log guard sees `thread '…' panicked`.
- `thegn session snapshot --text` → the ANSI repaint fed through the pane
  emulator and `copymode::extract`.
- Pinned muse: `flake.nix` input `muse` (built as `packages.muse`, on the dev
  shell PATH). Bump with `nix flake update muse`; the specs need the
  `feat/agent-session` line or newer.
