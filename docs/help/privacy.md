---
id: privacy
title: Privacy and network use
order: 40
---

# Privacy and network use

thegn is a local terminal tool. This page describes what it writes on your
machine and when thegn or a tool it starts can contact the network.

No vendor telemetry was found: the source contains no analytics or crash-upload
client. thegn does not check for new versions. A first-run network trace script
exists in the source tree (`scripts/first-run-net-trace.sh`, run with
`just net-trace`); its results are pending, so this page does not claim that
thegn makes no network calls.

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
- **Doctor bundles:** `thegn doctor bundle --out FILE` creates a local gzip
  archive with a doctor report, redacted config, retained crash reports and
  bounded log tails (up to 500 lines per log). It does not upload the archive.
  Review its manifest and the extracted contents before sharing it. See
  [[debugging]].
- **Secrets:** `thegn config show` masks secret-named values. `thegn secret set`
  stores tokens in the operating system keyring; config can also refer to
  secrets through `keyring:`, `env:` and `file:` references. See
  [[configuration]].
- **Sandboxed panes:** The default profile is `sealed`, which hides host
  `$HOME`, including its credentials, from sandboxed panes. The opt-in
  `standard` profile leaves `$HOME` readable, including credentials. Native
  plugins run as you without sandboxing and may do what your account can do;
  thegn cannot tell you what they send. See [[sandboxing]] and [[plugins]].

## What can leave the machine, and when

### On by default, when the matching local setup exists

- **Git fetches — `[git] auto_fetch = true`:** thegn runs background
  `git fetch --prune` against remotes already configured in a repository, using
  your Git credentials; it never merges, skips fetches while offline, and can
  be stopped with `auto_fetch = false`.
- **Forge CLIs — `[ci] provider = "auto"` and `[pr]`:** GitHub PR/CI and
  GitLab CI refreshes use `gh` or `glab` when available, with their existing
  authentication, to read PR/CI data; a missing `gh` disables the GitHub CLI
  integration.
- **AI-account usage — `[usage] enabled = true` and `allow_network = true`:**
  Codex usage is read from local files, while Claude and Antigravity usage may
  use an authenticated request with the OAuth token already stored by that
  tool; polling defaults to 300 seconds and is floored at 60. Turning off
  `allow_network` stops those requests and shows those rows as unavailable;
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
  Remote, provider, placement and voice features require their respective
  settings and are experimental and off in the stable channel; voice model
  downloads require your size confirmation. See [[daemon-and-sessions]],
  [[mobile-access]] and [[release-channels]].

## How to check for yourself

Run `thegn doctor` to inspect the channel, log sinks and crash reports. Run
`thegn config show` to inspect effective settings with secret-named values
masked. Generic operating-system tools such as `ss -tpn` and
`strace -f -e trace=connect` can also help you inspect network connections.
