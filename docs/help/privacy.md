---
id: privacy
title: Privacy and network use
order: 40
---

# Privacy and network use

thegn is a local terminal tool. This page describes what it writes on your
machine and when thegn or a tool it starts can contact the network.

No vendor telemetry was found: the source contains no analytics or crash-upload
client. Stable builds check for new versions once per day by default. A
first-run network trace script exists in the source tree
(`scripts/first-run-net-trace.sh`, run with `just net-trace`). It traces the release binary's first frame with `strace` and
lists every `connect()` target. A trace of main commit `1c5b3de0ab` showed only
local Unix-socket connects (the session bus was disabled and the user systemd
socket is a Unix socket). It showed no TCP connection to loopback or elsewhere.
The trace covers the first frame only; it does not show network use after that,
and it cannot see setuid helpers. So this page does not claim that thegn makes
no network calls at all.

The first-run trace used to show one TCP connect to the default MPD address
(`127.0.0.1:6600`). That probe is gone: `[media] backend = "auto"` no longer
dials it (see [[media]]).

## What stays on your machine

- **Crash reports:** By default, a panic writes a report under
  `$XDG_STATE_HOME/thegn/crash`. The crash directory is restricted to your
  account (0700) and reports to your account (0600). `crash_retention` defaults
  to 10 reports. `[diagnostics] crash_sink` is reserved: external forwarding
  is not implemented, and a non-empty value is rejected when config loads.
- **Logs:** Tracing files are written when `THEGN_LOG` requests them or
  `[log] file = true`; the default is `file = false`. While the compositor owns
  the screen, raw stderr is captured separately in `thegn-stderr.log`.
  `thegn logs tail` prints local log files. See [[debugging]].
- **State database:** `$XDG_STATE_HOME/thegn/thegn.db` stores workspace,
  worktree, pane state and caches. Its directory is restricted to your account
  and the file is restricted to your account (0600). The database also holds
  some live bearer credentials (iroh pair secrets), so treat copies and backups
  as sensitive.
- **Copied text:** Text you copy from a pane is kept in memory only (yank
  registers) and is not written to the state database. `[clipboard]
persist_registers = true` opts in to keeping it across restarts; turning it
  off again deletes the stored registers at the next startup. The `"+`
  system-clipboard register is never stored. See [[copy-and-select]].
- **Doctor report:** `thegn doctor` redacts identifying tailnet detail —
  the tailnet name and the control URL print as `***redacted***` — because
  its output is regularly pasted into issues. `thegn doctor --full` (alias
  `--unredacted`) prints them verbatim; `thegn doctor bundle` always
  archives the redacted view. See [[debugging]].
- **Doctor bundles:** `thegn doctor bundle --out FILE` creates a local gzip
  archive with a doctor report, redacted config, retained crash reports and
  bounded log tails (up to 500 lines per log). It does not upload the archive.
  Review its manifest and the extracted contents before sharing it. See
  [[debugging]].
- **Secrets:** `thegn config show` masks secret-named values. `thegn secret set`
  stores tokens in the operating system keyring; config can also refer to
  secrets through `keyring:`, `env:` and `file:` references. See
  [[configuration]].
- **Sandboxed panes:** The default profile is `standard`: sandboxed panes have a
  read-only root and `$HOME`, can read credentials there, and have network
  access. `sealed` is opt-in through `profile = "sealed"` in user config or an
  `[env.<name>.sandbox]` table, or `THEGN_SANDBOX_PROFILE=sealed`; it hides
  `$HOME`, disables networking, and drops all capabilities. Native plugins run
  as you without sandboxing and may do what your account can do; thegn cannot
  tell you what they send. See [[sandboxing]] and [[plugins]].

## What can leave the machine, and when

### On by default, when the matching local setup exists

- **Release availability — `[update] check = true`:** a background,
  unauthenticated request reads the latest public release tag from the GitHub
  releases API for `blakeashleyjr/thegn`. The result is cached under
  `$XDG_STATE_HOME/thegn` for one day; no account credentials are sent. Set
  `[update] check = false` to disable it. Dev-channel builds skip this check unless `check = true` is set explicitly.

- **Git fetches — `[git] auto_fetch = true`:** thegn runs background
  `git fetch --prune` against remotes already configured in a repository, using
  your Git credentials; it never merges, skips fetches while offline, and can
  be stopped with `auto_fetch = false`.
- **Forge CLIs — `[ci] provider = "auto"` and `[pr]`:** GitHub PR/CI and
  GitLab CI refreshes use `gh` or `glab` when available, with their existing
  authentication, to read PR/CI data; a missing `gh` disables the GitHub CLI
  integration.
- **AI-account usage — `[usage] enabled = true`:** Codex usage is read from
  local files. Claude and Antigravity usage use an authenticated request with
  the OAuth token already stored by that tool **only if you set
  `allow_network = true`**; it is off by default, so thegn never uses your
  existing login until you opt in. Polling defaults to 300 seconds and is
  floored at 60. With `allow_network` off those rows show as unavailable;
  turning off `enabled` removes the feature. thegn does not ask for or store an
  API key for this feature.
- **Cover art — `[media] show_art = true`:** when the Now-Playing panel receives
  an HTTP(S) artwork URL from a media player, thegn fetches the image off the UI
  loop with bounded size and address and redirect checks.
  `[media] enabled = false` turns media off.

### Only if you enable or run it

- **Weather — `[weather] enabled = false`:** when enabled, the wttr.in provider
  receives the configured `location`, or no location value if it is empty and
  the provider infers a city from your request IP.
- **Media — `[media] backend = "auto"`:** `auto` talks only to local sources
  (the session D-Bus for MPRIS, a live mpv socket). It does not dial the
  default MPD address (`127.0.0.1:6600`), so a first run opens no TCP
  connection. MPD is contacted only when you set `backend = "mpd"`, set
  `[media.mpd] socket`, or export `MPD_HOST`.
- **Issue trackers — `[issues] providers = []` and `provider = "none"`:**
  Linear, Jira, Kaneo and GitHub contact their APIs when you configure a
  provider; calendar network access requires a configured `calendar.accounts`
  CalDAV or ICS account.
- **Metrics — `metrics.targets` entries:** thegn scrapes the Prometheus URLs you
  configure, or runs a configured command collector.
- **Downloads you start:** `thegn debug setup` can download a pinned,
  SHA-256-verified BugStalker release from GitHub; `thegn mcp install <name>`
  acquires a configured server only when its matching grant permits it;
  `./install.sh --batteries` downloads a SHA-256-pinned Nerd Font archive and
  may build Alacritty.
- **Sandbox images — `[sandbox] image` and your container engine:** when a
  sandboxed pane needs an OCI image, your container engine pulls the configured
  or built-in base image; a devcontainer build runs the Dockerfile you provide.
  The fetch is performed by your container engine.
- **Plugins — plugin configuration:** plugins are ordinary programs running
  unsandboxed as your account, so they may make their own network requests;
  thegn cannot tell you what they send. See [[plugins]].
- **Model proxy — `[model_proxy]`:** `tgproxy` is an opt-in component you
  configure with provider endpoints and secret references; it is not part of
  the stable shell.
- **Sharing and serving — `thegn serve`, `[share]`, and dev-channel settings:**
  running `thegn serve` listens on loopback by default; `[share] provider` is
  `"none"` by default, and a configured tunnel connects to other machines.
  Remote, provider and placement features require their respective settings
  and are experimental and off in the stable channel. Voice is available on
  stable but off until you enable it; voice model downloads require your size
  confirmation. See [[daemon-and-sessions]],
  [[mobile-access]] and [[release-channels]].

## How to check for yourself

Run `thegn doctor` to inspect the channel, log sinks and crash reports. Run
`thegn config show` to inspect effective settings with secret-named values
masked. Generic operating-system tools such as `ss -tpn` and
`strace -f -e trace=connect` can also help you inspect network connections.
