---
id: troubleshooting
title: Troubleshooting
order: 42
---

# Troubleshooting

Start with the symptom you see. For more diagnostics, see [[debugging]].

## thegn will not start, says “command not found,” or shows a loader error

**Why:** The GNU archive needs a glibc version at least as new as the build
runner. The archive contains `thegn` only (no `tg` alias), and Git is
required at runtime.

**Do this:** On an older Linux distribution, use the musl archive. Put
`$HOME/.local/bin` on `PATH`; for tarball installs, also put `fzf`, `gum`,
`yazi`, `delta`, and `gdu` on `PATH` for the panes that use them. On macOS,
clear quarantine from a browser-downloaded archive with:

```sh
xattr -dr com.apple.quarantine ./thegn
```

## The screen shows boxes, wrong glyphs, or wrong colours

**Why:** The terminal may not support the glyph or colour mode selected by
thegn.

**Do this:** Run `thegn doctor` and check **Resolved capabilities**. Set
`[theme] glyphs = "ascii"` or `[theme] color = "mono"` in your config if
needed. `NO_COLOR` forces monochrome unless config explicitly sets a colour
mode; `THEGN_THEME_GLYPHS` and `THEGN_THEME_COLOR` override their settings.
See [[terminal-compatibility]].

## Alt/Option chords type symbols on macOS

**Why:** macOS terminals may use Option to compose characters instead of
sending Alt.

**Do this:** Set Option to send Meta/Alt in your terminal. Run `thegn doctor`
for the setting name; the terminal-specific steps are in
[[terminal-compatibility]].

## `Ctrl-1..9` or `Ctrl-Alt-1..9` do nothing

**Why:** These chords need xterm `modifyOtherKeys` level 2, which the terminal
may not report to thegn.

**Do this:** Check the `keyboard` row under **Resolved capabilities** in
`thegn doctor`. In tmux, enable extended keys:

```tmux
set -g extended-keys on
# tmux 3.4+ additionally:
set -as terminal-features '*:extkeys'
```

You can instead rebind the project or pin actions under `[keybinds]`; for
example, `summon-project-1 = "Ctrl Alt q"`. `Alt-1..9` work without this
terminal feature. See [[terminal-compatibility]].

## A key does nothing or a chord does something unexpected

**Why:** The key may not be bound in the current zone, or two bindings may
conflict.

**Do this:** Run `thegn keys list` (or `thegn keys list --zone sidebar`) to
inspect bindings, then `thegn keys validate` to check for conflicts. Press
F1 for help about the focused zone; see [[help]].

## Panes or sessions were lost after quitting or a crash

**Why:** Quit preserves daemon-backed panes, but panes that never used the
daemon do not survive. A saved pane that cannot reattach opens fresh, without
its previous process or scrollback.

**Do this:** Use **Quit and kill** when you intend to end the sessions. To run
panes in-process for one run, set `THEGN_NO_DAEMON=1`; `[daemon] enabled =
false` disables it in config. Crash reports are stored under
`$XDG_STATE_HOME/thegn/crash` and listed by `thegn doctor`. After a crash or
forced kill, the restored active tab may be one switch stale; see the Known
limitations help page.

## A “database schema vNN was written by a newer thegn” message or `DB REFUSED` banner appears

**Why:** An older thegn build is using a database migrated by a newer build.

**Do this:** Reinstall the newer build, run `thegn daemon stop`, then restart
the host and daemon from that build. See the Upgrading and uninstalling help
page.

## A sandboxed pane or agent launch is refused

**Why:** A refusal names its class and gives a one-line remediation. Common
causes include no backend meeting the default isolation floor, `bwrap` or
`systemd-run` missing from thegn's own `PATH`, a configured VPN with
`sealed-tunnel`, or a `sealed` launch that cannot hide `$HOME`. The default
`standard` profile protects integrity, not credentials: `$HOME` stays
readable. The opt-in `sealed` profile hides it; [[sandbox-profiles]] explains how to
choose one per worktree.

**Do this:** Inspect `thegn doctor` and the refusal remediation. On NixOS, the
sandboxing page recommends pinning `backend = "bwrap"`. See [[sandboxing]].

## A worktree or repository operation is refused by the merge queue

**Why:** A branch that moved after verification is refused.

**Do this:** Re-enqueue the branch with `thegn merge add` so the queue verifies
the new commits. See [[merge-queue]].

## thegn is slow or the fans spin

**Why:** The runtime profiler can show wake, rendering, and subsystem activity;
the live LOOP overlay shows a current window.

**Do this:** Set `THEGN_PERF=1` before launching the compositor to enable the
runtime rollup, or open the LOOP overlay in System → Telemetry. See
[[debugging]] for profiling details.

## Reporting a problem

**Why:** A doctor report and a reviewed support archive provide local
diagnostics without requiring a live daemon query.

**Do this:** Run `thegn doctor` or `thegn doctor --json`. To write a local
archive, run `thegn doctor bundle --out FILE`; review the manifest and
extracted files before sharing. Set `THEGN_LOG=info` to record the startup
waterfall in `$XDG_STATE_HOME/thegn/logs/thegn.log`. See [[debugging]].
