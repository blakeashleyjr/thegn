#!/usr/bin/env bash
# test/smoke.sh — hermetic, non-interactive end-to-end check of the native
# binary's CLI verbs (repos / recent / list / diff / pr / config) against a
# throwaway repo in an isolated HOME. Exits non-zero on any failure.
#
# The interactive compositor (worktree/agent/pin actions) is exercised by the
# host's own unit tests; this covers the shell-invocable surface.
#
# Usage: test/smoke.sh [path-to-thegn]   (defaults to ./target/debug/thegn)
set -euo pipefail

SZ="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/debug/thegn}"
# Resolve to an absolute path — the test cd's into a temp repo before running it.
SZ="$(cd "$(dirname "$SZ")" && pwd)/$(basename "$SZ")"
[[ -x $SZ ]] || {
  echo "not executable: $SZ (run: cargo build)" >&2
  exit 1
}

TMP="$(mktemp -d)"
# Best-effort teardown that preserves the real exit status. $HOME is inside
# $TMP, so if any check exercised a container backend, podman leaves overlay
# storage under $TMP/.local/share/containers/storage/overlay/ that this user
# cannot unlink. Under `set -e` a failing `rm -rf` in the trap became the
# script's exit code — CI reported "all smoke checks passed" and then failed
# the job. Never let cleanup decide the verdict.
cleanup() {
  local rc=$?
  rm -rf "$TMP" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT

export HOME="$TMP" XDG_CONFIG_HOME="$TMP/.config" XDG_STATE_HOME="$TMP/.local/state"
# Isolate the runtime dir too: the daemon control socket prefers
# $XDG_RUNTIME_DIR/thegn/daemon.sock, so leaving the real one exported would
# let the socket probe cross-connect these checks to a live daemon.
export XDG_RUNTIME_DIR="$TMP/run"
mkdir -p "$XDG_RUNTIME_DIR"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
# Exercise the full product surface: the experimental verbs (host/placement/
# kaneo) are dev-channel-only, so run smoke in the dev channel. A dedicated
# section below verifies the stable channel refuses them + clamps.
export THEGN_CHANNEL=dev

mkdir -p "$XDG_CONFIG_HOME/thegn"
cat >"$XDG_CONFIG_HOME/thegn/config.toml" <<EOF
worktrees_dir = "$TMP/wt"
name_scheme = "numbered"
repo_roots = ["$TMP/code"]

# A launch preset (item 165): 'thegn open --preset' validates the name and
# enqueues a name-only intent. NOTE: this heredoc is UNQUOTED (it interpolates
# \$TMP), so never write a \$(...) form here — even inside a comment the shell
# runs it and splices its output into the TOML.  See the tailnet note below.
[[presets]]
name = "dev"
description = "smoke preset"
commands = ["", "true"]
mode = "tabs"

# The lazygit-suite git keys must parse and validate.
[git]
override_gpg = true

[[git_commands]]
key = "p"
context = "branches"
command = "git push {{.SelectedBranch.Name | quote}}"
output = "popup"
prompts = [{ type = "input", title = "Remote", key = "Remote" }]

# Per-sandbox VPN config must parse + validate (provider sub-tables included).
[sandbox.vpn]
provider = "tailscale"
mode = "sidecar"
dns = "tunnel"

[sandbox.vpn.tailscale]
auth_key = "env:TS_AUTHKEY"

# Hosts-as-resources: a local-reach host + a host-backed env must parse,
# validate, and drive the thegn-host CLI (state stays in this temp HOME).
[host.smoke-local]
reach = "local"
install_runtime = "never"
volumes = []

# Inbound tailnet host discovery must parse; a bogus client binary makes
# 'thegn host discover' degrade DETERMINISTICALLY here (independent of whether a
# real tailscale is installed on the runner -- on NixOS it usually is, via
# /run/current-system/sw/bin).  This line once read "\$(host discover)", which
# the unquoted heredoc happily EXECUTED: the DNS 'host' tool spliced three
# ";; communications error" lines into config.toml, the file stopped parsing,
# thegn fell back to defaults -- tailscale_bin = "tailscale" -- and the
# missing-client check below failed.
[host_discovery.tailnet]
tailscale_bin = "/nonexistent/thegn-smoke-tailscale-xyz"

[env.smoke-hosted]
placement = "local"
host = "smoke-local"

# Ingress sharing config must parse + validate (all provider sub-tables).
[share]
provider = "bore"
allow_public = true

[share.frp]
server_addr = "frps.example.com"
subdomain_host = "share.example.com"

[share.tailscale]
funnel = false
EOF

fail=0
ok() { printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1"
  fail=1
}
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# Two repos under the scan root, plus one outside it.
mkdir -p "$TMP/code"
for n in alpha beta; do
  git init -q "$TMP/code/$n"
  git -C "$TMP/code/$n" commit -q --allow-empty -m init
done
R="$TMP/repo" # this one is OUTSIDE repo_roots
git init -q "$R"
git -C "$R" commit -q --allow-empty -m init
git -C "$R" branch -M main
cd "$R"

echo "thegn smoke test"

# Directory-agnostic repo discovery: finds the two repos under the scan root,
# and not the one outside it — regardless of $PWD.
check "repos discovers repos under repo_roots" \
  "[[ \$('$SZ' repos | wc -l) -eq 2 ]]"
check "discovery is scoped to repo_roots (excludes outside repos)" \
  "! '$SZ' repos | grep -q '/repo$'"

# config: effective value lookup + validation.
check "config get returns a known key" \
  "[[ -n \$('$SZ' config get picker) ]]"
check "config validate succeeds on the seeded config" \
  "'$SZ' config validate >/dev/null 2>&1"
# A malformed config is NOT a validate failure -- thegn warns and falls back to
# defaults, which validate cleanly. Every check that depends on a seeded key
# would then silently test the default instead, so assert the seed really parsed.
check "the seeded config parses (no fallback to defaults)" \
  "! '$SZ' config validate 2>&1 | grep -q 'parse error'"
check "config show emits TOML" \
  "'$SZ' config show | grep -q 'worktrees_dir'"
check "sandbox vpn config parses and surfaces the provider" \
  "'$SZ' config show | grep -q 'tailscale'"
check "config get reads a nested vpn key" \
  "[[ \$('$SZ' config get sandbox.vpn.provider 2>/dev/null) == 'tailscale' || -n \$('$SZ' config show | grep -A2 'sandbox.vpn') ]]"
# A pre-existing bad enum value in some OTHER key must NOT block setting an
# unrelated valid key (the whole-file re-validate only rolls back NEW errors).
# Isolated config dir so the seeded config above stays clean.
check "config set of a valid key survives a pre-existing bad value elsewhere" \
  "D=\$(mktemp -d); mkdir -p \"\$D/thegn\"; printf 'lifecycle.eager = \"bogus\"\n' > \"\$D/thegn/config.toml\"; XDG_CONFIG_HOME=\"\$D\" '$SZ' config set picker fzf >/dev/null 2>&1 && XDG_CONFIG_HOME=\"\$D\" '$SZ' config get picker | grep -q fzf"

# Model proxy (THE-58): opt-in and additive — disabled by default, nothing runs.
check "model proxy is disabled by default" \
  "'$SZ' proxy status --json | grep -q '\"enabled\":false'"
# SecretRef-only key custody: a raw literal api_key must fail `config validate`.
check "config validate rejects a raw-literal model_proxy api_key" \
  "D=\$(mktemp -d); mkdir -p \"\$D/thegn\"; printf '[model_proxy]\nenabled = true\n\n[[model_proxy.providers]]\nname = \"x\"\nkind = \"openai\"\nbase_url = \"https://x\"\napi_key = \"sk-raw-literal\"\n' > \"\$D/thegn/config.toml\"; ! XDG_CONFIG_HOME=\"\$D\" '$SZ' config validate >/dev/null 2>&1"

# `config get` resolves ANY dotted key, not a hand-maintained allowlist — the
# whole nested surface (every [merge_queue] key, ui.*, …) used to exit 1 with
# empty output while `config explain` resolved the same path fine.
check "config get reads a nested merge_queue key" \
  "[[ -n \$('$SZ' config get merge_queue.on_landed) ]]"
check "config get --json emits a real bool, not a quoted string" \
  "[[ \$('$SZ' config get merge_queue.auto_land --json) == 'true' ]]"
check "config get still errors on an unknown key" \
  "! '$SZ' config get merge_queue.no_such_key >/dev/null 2>&1"
# `config set` can write sequence values; every value used to be written as a
# TOML string, so array-typed keys had no CLI path at all.
check "config set writes a real TOML array" \
  "D=\$(mktemp -d); mkdir -p \"\$D/thegn\"; XDG_CONFIG_HOME=\"\$D\" '$SZ' config set merge_queue.regenerate_paths '[\"a.lock\", \"b.lock\"]' >/dev/null 2>&1 && XDG_CONFIG_HOME=\"\$D\" '$SZ' config get merge_queue.regenerate_paths --json | grep -q '\\[\"a.lock\",\"b.lock\"\\]'"
# Push-to-phone (THE-12): the command inbox must refuse to half-enable — an
# `enabled = true` with no SecretRef secret (and an empty allow list) is a
# startup config error, not a silent no-op. Isolated config dir so the seeded
# config stays clean.
check "config validate rejects a push inbox enabled without a secret" \
  "D=\$(mktemp -d); mkdir -p \"\$D/thegn\"; printf '[notifications.push.inbox]\nenabled = true\ntopic = \"cmd\"\n' > \"\$D/thegn/config.toml\"; ! XDG_CONFIG_HOME=\"\$D\" '$SZ' config validate >/dev/null 2>&1"
# A well-formed outbound push config (even pointed at an unreachable server)
# loads and renders green — the doctor probe is offline (no network round-trip),
# so nothing hangs.
check "config with an outbound push channel loads and doctor renders it" \
  "D=\$(mktemp -d); mkdir -p \"\$D/thegn\"; printf '[notifications.push]\nkind = \"ntfy\"\nserver = \"http://127.0.0.1:9\"\ntopic = \"t\"\n' > \"\$D/thegn/config.toml\"; XDG_CONFIG_HOME=\"\$D\" '$SZ' config validate >/dev/null 2>&1 && XDG_CONFIG_HOME=\"\$D\" '$SZ' doctor | grep -q 'push out'"
# Transport retry (THE-86): a `[pipeline.transport_retry]` override — including
# a REPLACED signature list — parses and validates. Isolated config dir so the
# seeded config stays clean.
check "config validate accepts a [pipeline.transport_retry] override" \
  "D=\$(mktemp -d); mkdir -p \"\$D/thegn\"; printf '[pipeline.transport_retry]\nenabled = true\nmax_attempts = 2\ntransport_signatures = [\"overloaded_error\", \"socket hang up\"]\nlimit_signatures = [\"weekly limit\"]\n' > \"\$D/thegn/config.toml\"; XDG_CONFIG_HOME=\"\$D\" '$SZ' config validate >/dev/null 2>&1"
# doctor surfaces the resolved paths, so a missing repo_root / a relocated $HOME
# is one glance instead of "you have no repos".
check "doctor reports a Paths section" \
  "'$SZ' doctor | grep -q '^Paths'"
check "doctor reports a Mobile access section" \
  "'$SZ' doctor | grep -q '^Mobile access'"
# The drawer's file-manager provider is a seam like every other backend: it
# reports a row in the Providers section (seam "files", provider "yazi" by
# default) with its availability + caps.
check "doctor reports the drawer file-manager provider" \
  "'$SZ' doctor | grep -q '^Providers' && '$SZ' doctor --json | grep -q '\"seam\": \"files\"'"

# Diagnostics: the identification block (version / channel / build / OS, daemon
# reachability, log sinks) in both text and JSON.
echo "diagnostics:"
check "doctor reports an Installation identification block" \
  "'$SZ' doctor | grep -q '^Installation' && '$SZ' doctor | grep -q '^Logs (\[log\])'"
check "doctor --json carries the identification block" \
  "'$SZ' doctor --json | grep -q '\"identification\"' && '$SZ' doctor --json | grep -q '\"version\"'"
check "doctor lists the log sinks with their caps" \
  "'$SZ' doctor | grep -q 'thegn-stderr.log' && '$SZ' doctor | grep -q 'thegn-daemon.log'"

# Completions health: doctor reports, per shell AND per command name, where a
# completion file is installed and whether it is current. Nothing is installed
# in the isolated HOME, which is also the common case on a CI runner — so this
# pins that an absent install is a report line carrying its fix command, never
# a doctor failure.
check "doctor reports a Completions section" \
  "'$SZ' doctor | grep -q '^Completions'"
check "doctor --json carries a completions row per shell and command" \
  "'$SZ' doctor --json | python3 -c 'import json,sys; r=json.load(sys.stdin)[\"completions\"]; assert {x[\"shell\"] for x in r} == {\"zsh\",\"bash\",\"fish\"}, r; assert {x[\"command\"] for x in r} == {\"thegn\",\"tg\"}, r'"
check "doctor exits 0 with no completions installed, and names the fix" \
  "'$SZ' doctor >/dev/null && '$SZ' doctor | grep -q 'absent.*run: thegn completions zsh > '"
# The value-source seam's third leg: a slot that deliberately does not complete
# says so, with its reason, instead of looking like a bug.
check "doctor names the reserved completion value sources" \
  "'$SZ' doctor | grep -qE '^  value sources +[0-9]+ live, [0-9]+ reserved$' \
     && '$SZ' doctor | grep -q 'branch .*reserved: git I/O'"

# A deliberate panic (test-only hook) must write a crash report even with no
# logging configured, recording the version, the process kind, and a backtrace.
# THEGN_LOG is explicitly unset so this truly exercises the no-sink path.
env -u THEGN_LOG -u THEGN_LOG_LEVEL THEGN_PANIC_TEST=1 "$SZ" doctor >/dev/null 2>&1 || true
CRASH_DIR="$XDG_STATE_HOME/thegn/crash"
check "a panic writes a crash report with logging off" \
  "ls '$CRASH_DIR'/*.txt >/dev/null 2>&1"
# NB: a distinct variable name (not the smoke-wide \$R repo path) — `check` evals
# in the current shell, so a bare `R=…` here would clobber it.
check "the crash report records version, proc kind, and a backtrace" \
  "CR=\$(ls -t '$CRASH_DIR'/*.txt | head -1); grep -q 'thegn crash report' \"\$CR\" && grep -q 'process:   cli' \"\$CR\" && grep -q 'backtrace:' \"\$CR\""

# The debug bundle: a redacted tar.gz with a printed manifest; a seeded token
# never appears in the archive.
BUNDLE="$TMP/thegn-bundle.tar.gz"
"$SZ" --set share.frp.token=SMOKE_SECRET_TOKEN doctor bundle --out "$BUNDLE" >"$TMP/bundle.out" 2>&1 || true
check "doctor bundle writes a gzip archive" \
  "test -s '$BUNDLE' && head -c2 '$BUNDLE' | od -An -tx1 | grep -q '1f 8b'"
check "doctor bundle prints a manifest naming its contents" \
  "grep -q 'thegn debug bundle' '$TMP/bundle.out' && grep -q 'doctor.json' '$TMP/bundle.out'"
check "the bundle redacts the seeded token (no plaintext secret)" \
  "mkdir -p '$TMP/bx' && tar xzf '$BUNDLE' -C '$TMP/bx' && ! grep -rq 'SMOKE_SECRET_TOKEN' '$TMP/bx' && grep -q 'redacted' '$TMP/bx/config.redacted.toml'"

# mcp serve: the read-only docs endpoint answers JSON-RPC over stdio.
check "mcp serve initialize reports the docs server" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}' | '$SZ' mcp serve | grep -q 'thegn-docs'"
check "mcp serve tools/list advertises search_docs" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}' | '$SZ' mcp serve | grep -q 'search_docs'"
check "mcp serve search_docs finds the merge-queue help page" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"search_docs\",\"arguments\":{\"query\":\"merge queue\"}}}' | '$SZ' mcp serve | grep -q 'merge-queue'"

# ── mcp proxy hub: end-to-end against a stub stdio MCP server ────────────────
# A tiny substring-matching MCP server (no JSON parser needed): it advertises
# two tools (echo, danger); the proxy config exposes only `echo`, so default-deny
# filtering + `<upstream>__<tool>` namespacing are both asserted below.
cat >"$TMP/stub-mcp.sh" <<'STUB'
#!/usr/bin/env bash
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"stub","version":"0"}}}\n' "$id" ;;
    *'"notifications/'*) : ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"e","inputSchema":{"type":"object"}},{"name":"danger","description":"d","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echoed"}]}}\n' "$id" ;;
    *) [ -n "$id" ] && printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"nope"}}\n' "$id" ;;
  esac
done
STUB
chmod +x "$TMP/stub-mcp.sh"
cat >>"$XDG_CONFIG_HOME/thegn/config.toml" <<EOF

[mcp_servers.stub]
command = ["bash", "$TMP/stub-mcp.sh"]

[mcp_servers.stub.proxy]
tools = ["echo"]
scope = "global"
EOF

check "mcp proxy initialize reports the proxy server" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}' | '$SZ' mcp proxy | grep -q 'thegn-mcp-proxy'"
check "mcp proxy namespaces the exposed tool" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}' | '$SZ' mcp proxy | grep -q 'stub__echo'"
check "mcp proxy default-deny hides the unexposed tool" \
  "! { printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}' | '$SZ' mcp proxy | grep -q 'stub__danger'; }"
check "mcp proxy routes a call to the exposed tool" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"stub__echo\",\"arguments\":{}}}' | '$SZ' mcp proxy | grep -q 'echoed'"
check "mcp proxy refuses a filtered tool call" \
  "printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"stub__danger\",\"arguments\":{}}}' | '$SZ' mcp proxy | grep -q 'no such tool'"
check "mcp list shows the exposed-vs-hidden proxy policy" \
  "'$SZ' mcp list | grep -q 'proxy: exposed'"
check "mcp emit --proxy is secret-free (no env block)" \
  "! { '$SZ' mcp emit --proxy | grep -q '\"env\"'; }"
check "mcp emit --proxy carries the proxy argv" \
  "'$SZ' mcp emit --proxy | grep -q '\"mcp\"'"
check "mcp preset list includes a local memory preset" \
  "'$SZ' mcp preset list | grep -q 'memory-graph'"
check "mcp preset show prints a config block" \
  "'$SZ' mcp preset show memory-graph | grep -q '\\[mcp_servers.memory-graph\\]'"
check "mcp secret list is empty and value-free by default" \
  "'$SZ' mcp secret list | grep -q 'no thegn-managed'"
check "mcp wire --agent claude writes a marked secret-free entry" \
  "'$SZ' mcp wire --agent claude >/dev/null 2>&1 && grep -q 'x-thegn-managed' \"\$HOME/.claude.json\" && ! grep -q '\"env\"' \"\$HOME/.claude.json\""
check "mcp wire --agent claude --remove is reversible" \
  "'$SZ' mcp wire --agent claude --remove >/dev/null 2>&1 && ! grep -q 'thegn' \"\$HOME/.claude.json\""
check "doctor reports the mcp proxy hub section" \
  "'$SZ' doctor | grep -q 'MCP proxy hub'"

# A hand-built worktree exercises diff/pr/list against real git state without
# the interactive host (worktree creation is a compositor action now).
WT="$TMP/wt/feature"
git -C "$R" worktree add -q -b feature "$WT" main
echo change >"$WT/f.txt"
git -C "$WT" add -A
git -C "$WT" commit -q -m work
echo more >>"$WT/f.txt"

check "diff emits without error" \
  "'$SZ' diff --worktree '$WT' --base main >/dev/null 2>&1"
check "diff --stat emits without error" \
  "'$SZ' diff --worktree '$WT' --base main --stat >/dev/null 2>&1"

# pr status degrades gracefully on a repo with no remote / no gh PR (exit 0).
check "pr status degrades gracefully (exit 0)" \
  "'$SZ' pr status --worktree '$WT' >/dev/null 2>&1"

# The in-app PR workflow's headless seams (comment / review / diff) parse and
# surface in --help (the acting paths need gh + a real PR, so only parsing is
# hermetic here).
check "pr comment subcommand parses" \
  "'$SZ' pr comment --help >/dev/null 2>&1"
check "pr review subcommand parses" \
  "'$SZ' pr review --help >/dev/null 2>&1"
check "pr diff subcommand parses" \
  "'$SZ' pr diff --help >/dev/null 2>&1"

# Hosts-as-resources CLI: list shows the seeded host; status renders an
# unprovisioned host; rm-cache refuses without --force and succeeds with it.
check "host list shows the seeded [host.*]" \
  "'$SZ' host list | grep -q smoke-local"
check "host status renders an unprovisioned host" \
  "'$SZ' host status smoke-local | grep -q unprovisioned"
check "host rm-cache refuses without --force" \
  "! '$SZ' host rm-cache smoke-local >/dev/null 2>&1"
check "host rm-cache --force succeeds" \
  "'$SZ' host rm-cache smoke-local --force >/dev/null 2>&1"

# Inbound tailnet discovery degrades cleanly with no usable tailscale client:
# non-zero exit, a message that NAMES the missing binary, and no panic. The
# client is the bogus [host_discovery.tailnet] tailscale_bin seeded above, so
# both checks hold whether or not the runner has a real tailscale on PATH.
check "host discover exits non-zero when the tailscale client is missing" \
  "! '$SZ' host discover >/dev/null 2>&1"
# Capture, then grep -- NOT `thegn ... | grep`: under `set -o pipefail` the
# command's (expected, asserted above) non-zero exit fails the whole pipeline
# even when grep matches, so the piped form could never go green.
check "host discover names the missing client (no panic)" \
  "out=\$('$SZ' host discover 2>&1) || true
   printf '%s' \"\$out\" | grep -q 'not found on PATH' &&
     printf '%s' \"\$out\" | grep -q 'thegn-smoke-tailscale-xyz' &&
     ! printf '%s' \"\$out\" | grep -qi 'panicked'"
check "host.discover is a catalog row (read scope), CLI surface" \
  "'$SZ' api list | grep -E '^host.discover' | grep -q 'read'"

# GOLDEN PATH (gated: needs podman + registry egress): a first provision does
# the work; the second must be a DB-only no-op that reports zero transfers
# (its event trail gains no new 'deliver' rows). TG_SMOKE_HOST_LIVE=1 enables.
if [[ ${TG_SMOKE_HOST_LIVE:-} == "1" ]] && command -v podman >/dev/null 2>&1; then
  check "host provision reaches ready (live)" \
    "'$SZ' host provision smoke-local </dev/null"
  DBH="$XDG_STATE_HOME/thegn/thegn.db"
  delivers_before="$(sqlite3 "$DBH" "SELECT count(*) FROM host_events WHERE step='deliver'")"
  check "second host provision is a no-op (live)" \
    "'$SZ' host provision smoke-local </dev/null"
  delivers_after="$(sqlite3 "$DBH" "SELECT count(*) FROM host_events WHERE step='deliver'")"
  check "second provision transferred nothing (golden path)" \
    "[[ '$delivers_before' -eq '$delivers_after' ]]"
else
  echo "  skip live host golden-path (set TG_SMOKE_HOST_LIVE=1 with podman + egress)"
fi

# ci (AV group): detection finds a seeded workflow file; runs/detect degrade
# gracefully with no remote/provider (exit 0, never crash).
mkdir -p "$WT/.github/workflows"
echo "on: push" >"$WT/.github/workflows/ci.yml"
check "ci detect finds the seeded GitHub Actions workflow" \
  "'$SZ' ci detect --worktree '$WT' | grep -q 'GitHub Actions'"
check "ci runs degrades gracefully (exit 0)" \
  "'$SZ' ci runs --worktree '$WT' >/dev/null 2>&1"

# list works against the DB (empty here is fine — must not error).
check "list runs without error" \
  "'$SZ' list >/dev/null 2>&1"
check "recent runs without error" \
  "'$SZ' recent >/dev/null 2>&1"

# ── CLI surface v2: wt/repo namespaces + headless lifecycle + --json ─────────
# Namespaced spellings mirror the (hidden but functional) legacy verbs.
check "wt list matches legacy list" \
  "[[ \"\$('$SZ' wt list)\" == \"\$('$SZ' list)\" ]]"
check "repo list matches legacy repos" \
  "[[ \"\$('$SZ' repo list)\" == \"\$('$SZ' repos)\" ]]"
check "repo recent matches legacy recent" \
  "[[ \"\$('$SZ' repo recent)\" == \"\$('$SZ' recent)\" ]]"

# Headless worktree lifecycle: create prints the path and registers in
# git + DB; removal cleans the checkout + DB rows and honors --delete-branch.
NP="$("$SZ" wt new smoke-cli --repo "$R")"
check "wt new prints an existing worktree path" "[[ -d '$NP' ]]"
check "wt new registered the branch in git" \
  "git -C '$R' worktree list --porcelain | grep -q 'smoke-cli'"
check "wt new appears in wt list" "'$SZ' wt list | grep -q 'smoke-cli'"
NB="$(git -C "$NP" symbolic-ref --short HEAD)"
NJ="$("$SZ" wt new smoke-json --repo "$R" --json)"
check "wt new --json emits branch+path" "printf '%s' \"\$NJ\" | grep -q '\"branch\"'"
NJ_PATH="$(grep -o '"path":"[^"]*"' <<<"$NJ" | cut -d'"' -f4)"
check "wt rm by branch name removes the checkout" \
  "'$SZ' wt rm '$NB' --force >/dev/null && [[ ! -d \$NP ]]"
check "wt rm keeps the branch by default" \
  "git -C '$R' rev-parse --verify --quiet 'refs/heads/$NB' >/dev/null"
check "wt rm --delete-branch drops the branch" \
  "'$SZ' wt rm '$NJ_PATH' --delete-branch --force >/dev/null && \
   [[ -z \$(git -C '$R' branch --list '*smoke-json*') ]]"
check "wt rm unknown target exits 3" \
  "'$SZ' wt rm no-such-thing --force >/dev/null 2>&1; [[ \$? -eq 3 ]]"
if command -v sqlite3 >/dev/null 2>&1; then
  DBS="$XDG_STATE_HOME/thegn/thegn.db"
  check "wt rm cleaned the DB worktree rows" \
    "[[ \$(sqlite3 \"$DBS\" \"SELECT count(*) FROM worktrees WHERE worktree LIKE '%smoke-cli%'\") -eq 0 ]]"
  check "wt rm left no tab_groups rows" \
    "[[ \$(sqlite3 \"$DBS\" \"SELECT count(*) FROM tab_groups WHERE worktree LIKE '%smoke-cli%'\") -eq 0 ]]"
fi

# Projects (THE-33): group two repos, batch-create a feature across both, and
# verify the retry/attach path. `alpha` + `beta` under $TMP/code are the members.
"$SZ" project create smoke-proj >/dev/null
"$SZ" project assign smoke-proj "$TMP/code/alpha" >/dev/null
"$SZ" project assign smoke-proj "$TMP/code/beta" >/dev/null
check "project list --json reports the two members" \
  "[[ \$('$SZ' project list --json | grep -o '\"members\":2' | head -1) == '\"members\":2' ]]"
check "project rm refuses a non-empty project without --force" \
  "! '$SZ' project rm smoke-proj >/dev/null 2>&1"

# Batched create: one linked branch name, a worktree in each member repo.
# shellcheck disable=SC2034 # read by the `check` bodies below, which run under `eval`
PJ="$("$SZ" wt new cross-feat --project smoke-proj --json)"
check "wt new --project emits a per-member report" \
  "printf '%s' \"\$PJ\" | grep -q '\"branch\"' && printf '%s' \"\$PJ\" | grep -q '\"status\":\"created\"'"
check "batched create made the branch in alpha" \
  "[[ -n \$(git -C '$TMP/code/alpha' branch --list '*cross-feat*') ]]"
check "batched create made the branch in beta" \
  "[[ -n \$(git -C '$TMP/code/beta' branch --list '*cross-feat*') ]]"

# Re-run attaches: both members already have the branch → reported exists,
# exit 0 (idempotent retry-after-partial-failure recovery path).
check "re-running --project attaches existing members and exits 0" \
  "'$SZ' wt new cross-feat --project smoke-proj --json | grep -q '\"status\":\"exists\"'"

# Subset: --repos restricts creation to the named member(s) only.
# shellcheck disable=SC2034 # read by the `check` bodies below, which run under `eval`
PJ2="$("$SZ" wt new subset-feat --project smoke-proj --repos beta --json)"
check "wt new --project --repos restricts to the named subset" \
  "printf '%s' \"\$PJ2\" | grep -q '\"repo\":\"beta\"' && ! printf '%s' \"\$PJ2\" | grep -q '\"repo\":\"alpha\"'"
check "batched create did not touch the excluded member" \
  "[[ -z \$(git -C '$TMP/code/alpha' branch --list '*subset-feat*') ]]"

# Assign none unprojects; the project can then be deleted.
"$SZ" project assign none "$TMP/code/alpha" >/dev/null
"$SZ" project assign none "$TMP/code/beta" >/dev/null
check "project rm removes an emptied project" \
  "'$SZ' project rm smoke-proj >/dev/null && ! '$SZ' project list | grep -q smoke-proj"

# Repo map: `thegn map` builds a capped tree-sitter entity index from the git
# listing and renders a ranked, budgeted outline (no language server needed).
# Commit an entity-bearing file first (gpgsign off for hermetic signing-key-less
# runs). Inline crawl runs because the index is empty and no compositor owns it.
cat >"$R/mapfile.rs" <<'RS'
pub struct Widget {
    x: i32,
}
pub fn render_widget(w: &Widget) -> i32 {
    w.x
}
RS
git -C "$R" -c commit.gpgsign=false add mapfile.rs
git -C "$R" -c commit.gpgsign=false commit -q -m "add mapfile"
check "map lists an indexed entity" \
  "'$SZ' map --worktree '$R' | grep -q 'render_widget'"
check "map ranks the struct (kind fallback) into the outline" \
  "'$SZ' map --worktree '$R' | grep -q 'struct Widget'"
check "map --json emits rows with kind+name" \
  "'$SZ' map --worktree '$R' --json | grep -q '\"name\":\"render_widget\"'"
check "map --json reports indexable files present" \
  "'$SZ' map --worktree '$R' --json | grep -q '\"has_indexable_files\":true'"
check "map --file narrows to one file's outline" \
  "'$SZ' map --worktree '$R' --file mapfile.rs | grep -q 'Widget'"

# Machine-readable output: one parseable JSON document per list surface.
check "list --json emits a JSON array" \
  "'$SZ' list --json | head -c1 | grep -q '\['"
check "env list --json parses" \
  "'$SZ' env list --json | head -c1 | grep -q '\['"
check "host list --json includes the seeded host" \
  "'$SZ' host list --json | grep -q smoke-local"
check "disk --json emits a JSON array" \
  "'$SZ' disk --json | head -c1 | grep -q '\['"
check "share list --json parses" \
  "'$SZ' share list --json | head -c1 | grep -q '\['"
check "forward list --json parses" \
  "'$SZ' forward list --json | head -c1 | grep -q '\['"
check "repo list --json parses" \
  "'$SZ' repo list --json | head -c1 | grep -q '\['"
check "ci runs --json degrades gracefully" \
  "'$SZ' ci runs --worktree '$WT' --json >/dev/null 2>&1"

# Grouped help + shell completions.
check "--help shows the Workspace group" "'$SZ' --help | grep -q 'Workspace:'"
check "--help shows the Forge group" "'$SZ' --help | grep -q 'Forge:'"
check "setup appears in --help (onboarding wizard)" \
  "'$SZ' --help | grep -q '^  setup '"
check "--help hides the legacy verbs" \
  "! '$SZ' --help | grep -qE '^  (repos|recent) '"
# Completions (THE-36). The default output is a REGISTRATION SHIM whose body
# calls back into the binary on every <TAB> — so each check greps for that
# shell's own registration marker, not for command names (the whole point is
# that the script contains none).
check "completions bash emits a bash registration" \
  "'$SZ' completions bash | grep -qE '^ *complete .* -F _clap_complete_thegn thegn$'"
check "completions zsh emits a zsh registration" \
  "'$SZ' completions zsh | grep -q '^compdef _clap_dynamic_completer_thegn thegn$'"
check "completions fish emits a fish registration" \
  "'$SZ' completions fish | grep -q '^complete .*--command thegn'"
# Every shim invokes the binary by NAME, never by the path it was generated
# from: the shipped scripts are produced in a Nix build sandbox / CI temp dir.
check "the shim calls thegn through PATH, not a build path" \
  "! '$SZ' completions zsh | grep -q '$SZ'"
# The documented degradation path if the unstable clap APIs ever break.
check "completions zsh --static emits a self-contained compdef" \
  "'$SZ' completions zsh --static | grep -q '^#compdef thegn'"
check "completions bash --static emits a self-contained script" \
  "'$SZ' completions bash --static | grep -qi complete"

# --- a <TAB> creates NO state ------------------------------------------------
# The load-bearing check of the whole feature: pressing <TAB> on a machine that
# has never run thegn must not create the state dir, the DB, or a WAL sidecar.
# The DB is opened SQLITE_OPEN_READ_ONLY precisely so this holds; a regression
# to Db::open() (which mkdir's, WAL-ifies and migrates) fails here.
CTAB="$TMP/completion-empty"
mkdir -p "$CTAB"
check "a completion request against an empty state root exits 0" \
  "env XDG_STATE_HOME='$CTAB' _CLAP_COMPLETE_INDEX=3 COMPLETE=zsh '$SZ' -- thegn wt rm '' >/dev/null 2>&1"
check "a completion request created no state" \
  "[[ -z \$(find '$CTAB' -mindepth 1 -print -quit) ]]"
check "a completion request printed nothing on stderr" \
  "[[ -z \$(env XDG_STATE_HOME='$CTAB' _CLAP_COMPLETE_INDEX=3 COMPLETE=zsh '$SZ' -- thegn wt rm '' 2>&1 >/dev/null) ]]"
# Structure still completes with no state at all (clap answers from the tree).
check "a completion request completes subcommands with no state" \
  "env XDG_STATE_HOME='$CTAB' _CLAP_COMPLETE_INDEX=1 COMPLETE=zsh '$SZ' -- thegn w | grep -q '^wt'"

# A word the shell hands over verbatim need not be valid UTF-8 — a latin-1 path
# is enough. `std::env::args()` PANICS on one, which used to print a Rust
# backtrace and exit 101; bash's and fish's shims (unlike zsh's) do not redirect
# stderr, so that landed on the user's prompt. A function, not an inline string:
# `check` eval's its argument and `$'\xff'` does not survive the round trip.
completion_survives_a_non_utf8_word() {
  local err rc
  err=$(env XDG_STATE_HOME="$CTAB" _CLAP_COMPLETE_INDEX=3 COMPLETE=bash \
    "$SZ" -- thegn wt rm $'/srv/caf\xe9/' 2>&1 >/dev/null)
  rc=$?
  [[ $rc -eq 0 && -z $err ]]
}
check "a non-UTF-8 word completes quietly instead of panicking" \
  completion_survives_a_non_utf8_word

# --- live values -------------------------------------------------------------
# The headline case: `wt rm <TAB>` names the worktrees still registered in the
# state DB at this point. This is the thing a static script can never do.
# Prefix-filtered to the worktrees dir, which also keeps the flags out of it —
# and asserted as "at least one, all under that dir" rather than against a
# hard-coded name, so it does not go stale when a check above adds or removes a
# worktree (the earlier `wt rm` checks already delete the ones they create).
check "completion offers real worktrees for 'wt rm'" \
  "_CLAP_COMPLETE_INDEX=3 COMPLETE=zsh '$SZ' -- thegn wt rm '$TMP/wt/' \
     | grep -qE '^$TMP/wt/[^:]+'"
check "completion prefix-filters worktrees" \
  "[[ -z \$(_CLAP_COMPLETE_INDEX=3 COMPLETE=zsh '$SZ' -- thegn wt rm /no-such-prefix/) ]]"
check "completion offers capability ids for 'api call'" \
  "_CLAP_COMPLETE_INDEX=3 COMPLETE=zsh '$SZ' -- thegn api call worktrees. | grep -q '^worktrees.list'"
check "completion offers config keys for 'config set'" \
  "_CLAP_COMPLETE_INDEX=3 COMPLETE=zsh '$SZ' -- thegn config set theme.acc | grep -q '^theme.accent'"
# The budget is honoured: an impossibly small one yields nothing rather than
# an error, a hang, or a backtrace.
check "an exhausted completion budget completes nothing, quietly" \
  "[[ -z \$(env THEGN_COMPLETE_BUDGET_MS=1 _CLAP_COMPLETE_INDEX=3 COMPLETE=zsh '$SZ' -- thegn api call worktrees. 2>&1) ]]"

# A coarse ceiling as a CANARY, not a perf gate — wall-clock gates stay out of
# `just ci` per the repo's perf policy (CLAUDE.md). 300ms is ~6x the observed
# debug-build cost; it fires on a structural regression (a full config load, a
# DB migration, a subprocess), not on a slow machine.
# A function rather than an inline expression: `check` eval's its argument, and
# a `$(…)` in that string is a standoff between shfmt (which rewrites it to
# single quotes) and SC2016 (which then wants it expanded).
completion_under_300ms() {
  local start end
  start=$(date +%s%N)
  _CLAP_COMPLETE_INDEX=3 COMPLETE=zsh "$SZ" -- thegn wt rm '' >/dev/null 2>&1
  end=$(date +%s%N)
  [[ $(((end - start) / 1000000)) -lt 300 ]]
}
check "a completion request answers well under 300ms (canary)" completion_under_300ms

# open: workspace pointer + repo-name resolution (no TUI launch in smoke;
# the live-instance intent path is unit-tested in core + verified manually).
check "open --no-launch sets the active-workspace pointer" \
  "'$SZ' open '$TMP/code/alpha' --no-launch >/dev/null"
check "open resolves a repo by basename" \
  "'$SZ' open alpha --no-launch >/dev/null"
check "open unknown repo exits 3" \
  "'$SZ' open no-such-repo --no-launch >/dev/null 2>&1; [[ \$? -eq 3 ]]"
# open --preset: validate against config, enqueue a NAME-ONLY launch intent.
check "open --preset unknown exits 3" \
  "'$SZ' open alpha --preset no-such-preset --no-launch >/dev/null 2>&1; [[ \$? -eq 3 ]]"
check "open --preset dev is accepted" \
  "'$SZ' open alpha --preset dev --no-launch >/dev/null 2>&1"
if command -v sqlite3 >/dev/null 2>&1; then
  check "open recorded alpha as the active workspace" \
    "sqlite3 \"$XDG_STATE_HOME/thegn/thegn.db\" \
       \"SELECT value FROM ui_state WHERE key='active_workspace'\" | grep -q alpha"
  check "open --preset enqueued a name-only launch_preset intent" \
    "sqlite3 \"$XDG_STATE_HOME/thegn/thegn.db\" \
       \"SELECT payload FROM intents WHERE kind='launch_preset'\" | grep -q '\"name\":\"dev\"'"
fi
# `open` is what writes the `repos` rows, so the repo-derived completion check
# lives here rather than up in the completions block.
check "completion offers real repos for 'open'" \
  "_CLAP_COMPLETE_INDEX=2 COMPLETE=zsh '$SZ' -- thegn open '' | grep -q '^alpha'"

# Named execution environments: list the library and resolve one for a worktree.
check "env list reports the default env" \
  "'$SZ' env list | grep -q 'default env:'"
check "env show resolves an environment for a worktree" \
  "'$SZ' env show '$WT' | grep -q '^env:'"
check "env set/show round-trips a selection" \
  "'$SZ' env set company-k8s '$WT' >/dev/null 2>&1 && '$SZ' env show '$WT' >/dev/null 2>&1"
# The canonical `--worktree` flag form (the positionals above stay as the
# hidden back-compat spelling — see docs/cli.md "Worktree targeting").
check "env show accepts the canonical --worktree flag" \
  "'$SZ' env show --worktree '$WT' | grep -q '^env:'"
# Round-trip: the flag-form WRITE must land on the SAME worktree a positional
# READ resolves (cross-checks that --worktree isn't silently dropped in favor of
# the cwd — `env set` exits 0 even when the selection is wrong, so assert the
# effect, not just the exit code). `env set` persists the selection unconditionally
# and the read preserves the requested name, so this leaves $WT carrying
# company-k8s exactly as the sandbox-argv check below documents.
check "env set --worktree lands the selection (flag-write / positional-read round-trip)" \
  "'$SZ' env set company-k8s --worktree '$WT' >/dev/null 2>&1 && '$SZ' env show '$WT' | grep -q company-k8s"
# Passing BOTH forms is a usage error: non-zero exit (no specific code promised).
check "env show refuses --worktree plus a positional (non-zero)" \
  "! '$SZ' env show --worktree '$WT' '$WT' >/dev/null 2>&1"
# The conflict guard is per-verb (flatten site): spot-check a second verb.
check "placement explain refuses --worktree plus a positional (non-zero)" \
  "! '$SZ' placement explain --worktree '$WT' '$WT' >/dev/null 2>&1"
# sandbox-argv takes the same flag and resolves like every other scoped verb.
# Target the repo root ($WT now carries the deliberately undefined company-k8s
# env selection, which launch_spec correctly refuses) and switch the seeded
# tailscale VPN off for this one call — its auth key is unset here by design.
check "sandbox-argv accepts the canonical --worktree flag" \
  "'$SZ' --set sandbox.vpn.provider=none sandbox-argv --worktree '$R' | grep -q ."
# No-arg default resolution: with $THEGN_WORKTREE exported to the repo root, a
# flag-less sandbox-argv resolves to it (the shared chain, not the old raw cwd).
# Run from a scratch cwd to prove $THEGN_WORKTREE — not the cwd — wins.
check "sandbox-argv with no --worktree resolves via the THEGN_WORKTREE env" \
  "(cd / && THEGN_WORKTREE='$R' '$SZ' --set sandbox.vpn.provider=none sandbox-argv | grep -q .)"

# ── merge queue (`merge` namespace, the fold-actor) ──────────────────────────
# Assign a worktree branch to the queue and drain it: a clean branch folds onto
# the target and lands (no agent needed). Exercises the CLI + DB round-trip.
check "merge list starts empty" \
  "'$SZ' merge list | grep -qi 'empty'"
MP="$("$SZ" wt new smoke-merge --repo "$R")"
MB="$(git -C "$MP" symbolic-ref --short HEAD)"
echo hi >"$MP/smoke-merge.txt"
git -C "$MP" add -A && git -C "$MP" commit -q -m "smoke merge change"
check "merge add queues the worktree branch" \
  "'$SZ' merge add '$MP' | grep -q 'queued'"
check "merge list shows the queued branch" \
  "'$SZ' merge list | grep -q '$MB'"
if command -v sqlite3 >/dev/null 2>&1; then
  check "merge add wrote a queued row" \
    "[[ \$(sqlite3 \"$XDG_STATE_HOME/thegn/thegn.db\" \
       \"SELECT count(*) FROM merge_queue WHERE branch='$MB' AND status='queued'\") -eq 1 ]]"
fi
# `merge rm` removes a queued entry by path; re-add so drain has work. Done while
# the worktree still exists — a clean land now auto-removes it (see below).
check "merge rm deletes the entry by the same path" \
  "'$SZ' merge rm '$MP' >/dev/null 2>&1"
# Flag-form twin: `merge rm` on a non-queued path exits non-zero, so it needs
# its own preceding add before the canonical `--worktree` removal.
check "merge add queues the branch again for the flag-form rm" \
  "'$SZ' merge add '$MP' | grep -q 'queued'"
check "merge rm accepts the canonical --worktree flag" \
  "'$SZ' merge rm --worktree '$MP' >/dev/null 2>&1"
check "merge add re-queues the branch after rm" \
  "'$SZ' merge add '$MP' | grep -q 'queued'"
check "merge drain lands the clean branch" \
  "'$SZ' merge drain | grep -q 'landed'"
check "drain advanced the target to include the branch's commit" \
  "git -C '$R' log --oneline | grep -q 'smoke merge change'"
# organize_folders + on_landed = "expire" are on by default: a clean land KEEPS
# the merged worktree and its branch, filed into merged_folder, until the
# merged_ttl_secs grace period is up. The worktree directory holds gitignored
# state that exists nowhere else, so landing must not be what deletes it.
check "clean land keeps the merged worktree during its grace period" \
  "[[ -d '$MP' ]]"
check "clean land keeps the merged branch during its grace period" \
  "[[ -n \$(git -C '$R' branch --list '$MB') ]]"
check "the landed row survives as the grace-period clock" \
  "[[ \$(sqlite3 \"$XDG_STATE_HOME/thegn/thegn.db\" \
     \"SELECT count(*) FROM merge_queue WHERE branch='$MB' AND status='landed'\") -eq 1 ]]"
# A sweep before the period is up must collect nothing — the default ttl is a
# week, and an expiry that fires early is the bug the grace period exists to stop.
check "sweep leaves a worktree that is not yet due" \
  "'$SZ' merge sweep | grep -q 'Nothing to sweep' && [[ -d '$MP' ]]"
# --force is the "clear merged now" gesture: same collection, ignoring the clock.
check "sweep --force removes the merged worktree" \
  "'$SZ' merge sweep --force | grep -q 'swept' && [[ ! -d '$MP' ]]"
check "sweep --force deletes the merged branch" \
  "[[ -z \$(git -C '$R' branch --list '$MB') ]]"

# `--json` must emit EXACTLY one document on every path. The empty queue is the
# case a cron/CI loop hits most often, and it used to print prose ("Nothing to
# drain.") with no JSON at all.
check "merge drain --json emits JSON on the empty queue" \
  "'$SZ' merge drain --json 2>/dev/null | python3 -c 'import json,sys; json.load(sys.stdin)'"

# ── PR queue (`pr queue`, team mode) ────────────────────────────────────────
# No forge is reachable in the smoke env, so this covers the CLI contract that
# does NOT need one: the disabled-by-default guard, the DB round-trip through
# `list`/`clear`, and the `--json` shapes. The classify/decide logic that needs
# a live PR is unit-tested in `thegn_core::pr_queue` instead.
check "pr queue refuses while disabled, naming the key to set" \
  "! '$SZ' pr queue list >/dev/null 2>&1"
# Capture the output so the message check works under `set -e`/pipefail even
# though the command is expected to fail (same idiom as the share guard below).
check "pr queue's refusal names the key to set" \
  "out=\$('$SZ' pr queue list 2>&1) || printf '%s' \"\$out\" | grep -q 'pr_queue'"
PRQ="--set pr_queue.enabled=true"
check "pr queue list starts empty once enabled" \
  "'$SZ' $PRQ pr queue list | grep -qi 'empty'"
check "pr queue list --json emits JSON on the empty queue" \
  "'$SZ' $PRQ pr queue list --json 2>/dev/null | python3 -c 'import json,sys; json.load(sys.stdin)'"
check "pr queue status --json emits JSON on the empty queue" \
  "'$SZ' $PRQ pr queue status --json 2>/dev/null | python3 -c 'import json,sys; json.load(sys.stdin)'"
check "pr queue drain --json emits JSON on the empty queue" \
  "'$SZ' $PRQ pr queue drain --json 2>/dev/null | python3 -c 'import json,sys; json.load(sys.stdin)'"
check "pr queue clear is a no-op on an empty queue, not an error" \
  "'$SZ' $PRQ pr queue clear | grep -q '0 removed'"
# `add` needs a real PR, so it must fail cleanly (not panic) with none.
check "pr queue add reports there is no pull request rather than crashing" \
  "! '$SZ' $PRQ pr queue add >/dev/null 2>&1"
# The schema migration created the table on this fresh DB.
if command -v sqlite3 >/dev/null 2>&1; then
  check "the pr_queue table exists after migration" \
    "[[ \$(sqlite3 \"$XDG_STATE_HOME/thegn/thegn.db\" \
       \"SELECT count(*) FROM sqlite_master WHERE type='table' AND name='pr_queue'\") -eq 1 ]]"
fi

# An unrunnable gate is an ENVIRONMENT failure, not a verdict about the branch:
# it must record `gate_error` (never `gate_failed`/"breaks build") and must not
# dispatch the fixing agent — which would set a coding model loose on source
# code in response to `command not found`.
GP="$("$SZ" wt new smoke-gate --repo "$R")"
GB="$(git -C "$GP" symbolic-ref --short HEAD)"
echo hi >"$GP/smoke-gate.txt"
git -C "$GP" add -A && git -C "$GP" commit -q -m "smoke gate change"
"$SZ" merge add "$GP" >/dev/null 2>&1
AGENT_MARK="$XDG_STATE_HOME/agent-fired"
check "an unrunnable gate is reported as gate_error, not a build verdict" \
  "'$SZ' --set merge_queue.gate_on=true \
     --set merge_queue.gate_command=definitely-not-a-real-binary \
     --set merge_queue.conflict_handoff=agent \
     --set merge_queue.agent_command=\"touch $AGENT_MARK\" \
     merge drain 2>&1 | grep -q 'NOT gated'"
check "the queue row records gate_error with the real reason" \
  "'$SZ' merge list | grep -q 'gate_error'"
check "an unrunnable gate never dispatches the fixing agent" \
  "[[ ! -e '$AGENT_MARK' ]]"
# The row must show the target the run actually folded into, not the one frozen
# at enqueue time (two different targets used to be shown for one operation).
check "merge list shows the run's effective target" \
  "'$SZ' merge list | grep -q '$GB'"
# Per-repo `[merge_queue]`: the whole table used to be global-only, so a repo
# whose gate or integration branch differed from the user's defaults could only
# be handled with `--set` flags on every invocation.
WSLUG="$(basename "$R")"
cat >>"$XDG_CONFIG_HOME/thegn/config.toml" <<EOF

[workspace.$WSLUG.merge_queue]
target_branch = "smoke-target"
EOF
check "a [workspace.<slug>] block refines merge_queue for that repo" \
  "[[ \$('$SZ' config explain merge_queue.target_branch --repo '$R' --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"value\"])') == 'smoke-target' ]]"
check "config explain names the workspace layer as the origin" \
  "'$SZ' config explain merge_queue.target_branch --repo '$R' | grep -q 'workspace'"
# Trim it again so the rest of the merge checks see the plain global config.
python3 - "$XDG_CONFIG_HOME/thegn/config.toml" <<'PYEOF'
import sys
p = sys.argv[1]
s = open(p).read()
i = s.rindex("[workspace.")
open(p, "w").write(s[:i])
PYEOF

# `merge retry` re-arms a blocked row (the CLI twin of the panel's `r`), and is
# a distinct non-zero outcome when there is nothing to re-arm.
check "merge retry re-queues a blocked row" \
  "'$SZ' merge retry --worktree '$GP' | grep -qi 're-queued'"
check "merge retry on an unqueued path exits non-zero" \
  "! '$SZ' merge retry --worktree '$R' >/dev/null 2>&1"
"$SZ" merge rm --worktree "$GP" >/dev/null 2>&1 || true

# ── placement engine ─────────────────────────────────────────────────────────
# Engine OFF (the default): the dry-run reports passthrough and no state is
# written — the byte-compatibility invariant's shell-visible face.
check "placement plan reports passthrough while the engine is off" \
  "'$SZ' placement plan '$R' | grep -q 'engine off'"
check "placement plan accepts the canonical --worktree flag" \
  "'$SZ' placement plan --worktree '$R' | grep -q 'engine off'"
check "placement list renders the seeded host (unknown size)" \
  "'$SZ' placement list | grep -q 'smoke-local'"
check "placement events is empty while the engine is off" \
  "'$SZ' placement events | grep -q 'no placement decisions'"
# Engine ON with a declared-capacity host: the broker's dry-run is
# deterministic — an unprobed host can't pack (no known runtime), so `auto`
# falls back to a dedicated placement on the empty box.
cat >>"$XDG_CONFIG_HOME/thegn/config.toml" <<EOF

[host.pool-box]
reach = "local"
install_runtime = "never"
volumes = []
capacity = { cpu = "8", memory = "16g" }

[placement]
enabled = true
EOF
check "placement plan decides deterministically with the engine on" \
  "'$SZ' placement plan '$R' --json | grep -q '\"decision\": \"dedicated\"'"
check "placement plan explains every candidate" \
  "'$SZ' placement plan '$R' --json | grep -q 'trust_class'"
# The dry-run must be side-effect free: no reservation, no event.
check "placement plan writes no decision events" \
  "'$SZ' placement events | grep -q 'no placement decisions'"
# Draining parks a host out of every lane: the plan flips to the other box.
check "host drain excludes the host from placement candidates" \
  "'$SZ' host drain pool-box >/dev/null 2>&1 && '$SZ' placement plan '$R' --json | grep -q 'draining'"
check "drained host refuses new provisioning" \
  "! '$SZ' host provision pool-box </dev/null >/dev/null 2>&1"
# Compute spend ledger: caps set/read + kill-switch round-trip.
check "placement budget sets and reads a cap" \
  "'$SZ' placement budget --set-limit 25 | grep -q '25.00'"
check "placement budget kill-switch round-trips" \
  "'$SZ' placement budget --kill | grep -q 'killed: true' && '$SZ' placement budget --unkill | grep -q 'killed: false'"

# ── ingress sharing (`[share]`) ──────────────────────────────────────────────
# The config parses (all provider sub-tables, exercised by validate above).
check "share config round-trips through config show" \
  "'$SZ' config show | grep -q 'allow_public'"
check "share list runs without error" \
  "'$SZ' share list >/dev/null 2>&1"

# Stubbed providers exercise the subprocess seam: `frpc`/`dumbpipe` on a private
# PATH stand in for the real binaries (each prints its line, then idles).
SHBIN="$TMP/shbin"
mkdir -p "$SHBIN"
cat >"$SHBIN/frpc" <<'STUB'
#!/usr/bin/env bash
echo "frpc started: $*"; sleep 30
STUB
cat >"$SHBIN/dumbpipe" <<'STUB'
#!/usr/bin/env bash
echo "to connect, use: dumbpipe connect-tcp TICKET123" >&2; sleep 30
STUB
chmod +x "$SHBIN/frpc" "$SHBIN/dumbpipe"

# frp: config-derived https subdomain URL + a materialized frpc.toml.
cat >"$TMP/share-frp.toml" <<EOF
[share]
provider = "frp"
[share.frp]
server_addr = "frps.example.com"
subdomain_host = "share.example.com"
EOF
PATH="$SHBIN:$PATH" "$SZ" --config "$TMP/share-frp.toml" share start 3000 --worktree "$WT" \
  >"$TMP/frp.out" 2>&1 &
FRP_PID=$!
for _ in $(seq 1 60); do
  if grep -q '→' "$TMP/frp.out" 2>/dev/null; then break; fi
  sleep 0.1
done
check "share frp derives the per-worktree https URL" \
  "grep -q 'https://feature-3000.share.example.com' '$TMP/frp.out'"
check "share frp materializes frpc.toml in the state dir" \
  "ls $XDG_STATE_HOME/thegn/share/*-3000/frpc.toml >/dev/null 2>&1"
kill "$FRP_PID" 2>/dev/null || true
wait "$FRP_PID" 2>/dev/null || true

# iroh: scrape the dumbpipe ticket into a copyable connect command.
printf '[share]\nprovider = "iroh"\n' >"$TMP/share-iroh.toml"
PATH="$SHBIN:$PATH" "$SZ" --config "$TMP/share-iroh.toml" share start 3000 --worktree "$WT" \
  >"$TMP/iroh.out" 2>&1 &
IROH_PID=$!
for _ in $(seq 1 60); do
  if grep -q '→' "$TMP/iroh.out" 2>/dev/null; then break; fi
  sleep 0.1
done
check "share iroh scrapes the dumbpipe ticket into a connect command" \
  "grep -q 'dumbpipe connect-tcp TICKET123' '$TMP/iroh.out'"
kill "$IROH_PID" 2>/dev/null || true
wait "$IROH_PID" 2>/dev/null || true

# allow_public safety guard: a public share is refused unless opted in.
cat >"$TMP/share-guard.toml" <<EOF
[share]
provider = "frp"
allow_public = false
[share.frp]
server_addr = "x"
subdomain_host = "y"
EOF
# A refused public share exits non-zero (misuse a script must detect) AND names
# the reason. Capture output so the message check works under `set -e`/pipefail
# even though the command is expected to fail.
check "share allow_public guard refuses public shares" \
  "out=\$('$SZ' --config '$TMP/share-guard.toml' share start 3000 --worktree '$WT' 2>&1) || printf '%s' \"\$out\" | grep -q 'public sharing is disabled'"

# Intent-first reach mapping: `--reach peer` resolves to the iroh provider.
cat >"$TMP/share-reach.toml" <<EOF
[share]
public = "frp"
team   = "tailscale"
peer   = "iroh"
[share.frp]
server_addr = "frps.example.com"
subdomain_host = "share.example.com"
EOF
PATH="$SHBIN:$PATH" "$SZ" --config "$TMP/share-reach.toml" share start 3000 --reach peer \
  --worktree "$WT" >"$TMP/reach.out" 2>&1 &
REACH_PID=$!
for _ in $(seq 1 60); do
  if grep -q '→' "$TMP/reach.out" 2>/dev/null; then break; fi
  sleep 0.1
done
check "share --reach peer resolves to the iroh provider" \
  "grep -q 'dumbpipe connect-tcp' '$TMP/reach.out'"
kill "$REACH_PID" 2>/dev/null || true
wait "$REACH_PID" 2>/dev/null || true

# An invalid reach is rejected: exit non-zero (misuse) with a message naming it.
check "share rejects an invalid --reach value" \
  "out=\$('$SZ' --config '$TMP/share-reach.toml' share start 3000 --reach bogus --worktree '$WT' 2>&1) || printf '%s' \"\$out\" | grep -q 'reach'"

# ── auto port forwarding (`[forward]`) ───────────────────────────────────────
# Config round-trips (the [forward] block parses + serializes) and the
# inspection CLI runs. Forwarding itself is driven by the live compositor's
# detector, so the bring-up path is exercised by the host unit tests + a
# live container check (below, guarded on podman); here we cover the CLI seam.
check "forward config round-trips through config show" \
  "'$SZ' config show | grep -q 'open_on_detect'"
check "forward list runs and reports an empty set" \
  "'$SZ' forward list 2>&1 | grep -q 'no forwards'"

# Seed a forward record and assert `forward list` renders the mapping + URL
# (exercises Db::upsert/list_forwards through the CLI read path).
if command -v sqlite3 >/dev/null 2>&1; then
  FDB="$XDG_STATE_HOME/thegn/thegn.db"
  "$SZ" forward list >/dev/null 2>&1 || true # ensure the DB + schema exist
  sqlite3 "$FDB" \
    "INSERT INTO forwards(worktree,container_port,host_port,url,created_at)
     VALUES('$WT',3000,8001,'http://127.0.0.1:8001',0);"
  check "forward list shows a remapped forward (container → host)" \
    "'$SZ' forward list 2>&1 | grep -q '3000 → 8001'"
  check "forward list shows the preview URL" \
    "'$SZ' forward list 2>&1 | grep -q 'http://127.0.0.1:8001'"
  check "forward stop removes the recorded forward" \
    "'$SZ' forward stop 3000 --worktree '$WT' >/dev/null 2>&1 && ! '$SZ' forward list 2>&1 | grep -q '3000 → 8001'"
else
  echo "  skip forward DB checks (sqlite3 not on PATH)"
fi

# v5 → v6 layout migration: seed a legacy flat tab_layout (pages as " ·N" name
# suffixes) into the state DB, open it once, and assert it transformed into
# worktree groups (tabs-within-a-worktree) with the legacy table dropped.
if command -v sqlite3 >/dev/null 2>&1; then
  DB="$XDG_STATE_HOME/thegn/thegn.db"
  mkdir -p "$(dirname "$DB")"
  sqlite3 "$DB" <<'SQL'
PRAGMA user_version = 5;
CREATE TABLE IF NOT EXISTS tab_layout (
  session_name TEXT, tab_name TEXT, kind TEXT, worktree TEXT,
  pane_tree TEXT, ordinal INTEGER, focused_pane INTEGER,
  PRIMARY KEY (session_name, tab_name));
INSERT INTO tab_layout VALUES
  ('/r', 'app/home',    'home',     '/r',       '{"leaf":0}', 0, 0),
  ('/r', 'app/feat',    'worktree', '/wt/feat', '{"leaf":1}', 1, 0),
  ('/r', 'app/feat ·2', 'worktree', '/wt/feat', '{"leaf":2}', 2, 0);
SQL
  "$SZ" list >/dev/null 2>&1 || true
  groups="$(sqlite3 "$DB" "SELECT count(*) FROM tab_groups WHERE session_name='/r'")"
  feat_tabs="$(sqlite3 "$DB" "SELECT count(*) FROM group_tabs WHERE session_name='/r' AND group_name='app/feat'")"
  legacy="$(sqlite3 "$DB" "SELECT count(*) FROM sqlite_master WHERE name='tab_layout'")"
  check "v5 tab_layout migrates into worktree groups (v6)" "[[ '$groups' -eq 2 ]]"
  check "page suffixes become tabs within the worktree" "[[ '$feat_tabs' -eq 2 ]]"
  check "legacy tab_layout is dropped after migration" "[[ '$legacy' -eq 0 ]]"
else
  echo "  skip v5→v6 migration check (sqlite3 not on PATH)"
fi

# ── control plane: pairing CRUD, no-daemon degradation, daemon lifecycle ────
echo "control plane:"

# Pairing management is pure DB — must work with NO daemon running.
# NOTE: command output is never interpolated into check()'s eval'd condition —
# the no-daemon message contains backticks, which eval would execute.
pair_out="$("$SZ" pair new --scope read,git --label smoke 2>&1)"
pair_url_ok=1
grep -q 'thegn://pair?' <<<"$pair_out" || pair_url_ok=0
pair_hash_ok=1
grep -q 'only its hash is stored' <<<"$pair_out" || pair_hash_ok=0
check "pair new mints a code and prints the pairing URL" "[[ $pair_url_ok -eq 1 ]]"
check "pair new never echoes a stored plaintext (hash-only note present)" \
  "[[ $pair_hash_ok -eq 1 ]]"
pair_id="$("$SZ" pair list --json | sed -n 's/.*"pairing_id": "\([a-f0-9]*\)".*/\1/p' | head -1)"
check "pair list --json surfaces the minted code" "[[ -n '$pair_id' ]]"
"$SZ" pair revoke "$pair_id" >/dev/null
pair_revoked_ok=1
"$SZ" pair list | grep -q revoked || pair_revoked_ok=0
check "pair revoke flips the state" "[[ $pair_revoked_ok -eq 1 ]]"

# Session verbs degrade clearly when no daemon is running (never crash).
set +e
nodaemon_out="$("$SZ" session list 2>&1)"
nodaemon_rc=$?
nodaemon_json="$("$SZ" session list --json 2>/dev/null)"
set -e
nodaemon_msg_ok=1
[[ $nodaemon_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$nodaemon_out" || nodaemon_msg_ok=0
nodaemon_json_ok=1
grep -q 'no_daemon' <<<"$nodaemon_json" || nodaemon_json_ok=0
check "session list without a daemon exits 1 with a clear message" \
  "[[ $nodaemon_msg_ok -eq 1 ]]"
check "session list --json emits the no_daemon error object" \
  "[[ $nodaemon_json_ok -eq 1 ]]"

# `thegn attach` (the local thin client) shares the same connect path, so it
# degrades identically when no daemon is running — never a crash.
set +e
attach_out="$("$SZ" attach 2>&1)"
attach_rc=$?
set -e
attach_ok=1
[[ $attach_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$attach_out" || attach_ok=0
check "attach without a daemon exits 1 with a clear message" \
  "[[ $attach_ok -eq 1 ]]"

# The agent-driving verbs (`session wait`/`session split`) share the connect
# path and must degrade cleanly too.
set +e
wait_out="$("$SZ" session wait --session bogus --until exited 2>&1)"
wait_rc=$?
split_out="$("$SZ" session split --session bogus 2>&1)"
split_rc=$?
set -e
verbs_ok=1
[[ $wait_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$wait_out" || verbs_ok=0
[[ $split_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$split_out" || verbs_ok=0
check "session wait/split without a daemon exit 1 with a clear message" \
  "[[ $verbs_ok -eq 1 ]]"

# --- agent orchestration surface (THE-57), daemon-free -----------------------
# The dispatch roster is local SQLite: an empty roster lists cleanly (JSON and
# human), and set-status validates its inputs (closed status set + real id)
# before touching the DB, so a supervisor never corrupts the ledger it resumes
# from.
check "dispatch list --json is a clean empty array" \
  "[[ \$('$SZ' dispatch list --json) == '[]' ]]"
check "dispatch list (human) says the roster is empty" \
  "'$SZ' dispatch list | grep -q 'no dispatches'"
check "dispatch set-status rejects a status outside the closed set" \
  "'$SZ' dispatch set-status 1 bogus >/dev/null 2>&1; [[ \$? -ne 0 ]]"
check "dispatch set-status rejects an unknown dispatch id" \
  "'$SZ' dispatch set-status 999999 done >/dev/null 2>&1; [[ \$? -ne 0 ]]"
# A configured stage backs the `session open --stage` offline refusals below
# (the prompt-refusal check needs a stage that EXISTS; the roster is DB-direct
# and unaffected by this block).
cat >>"$XDG_CONFIG_HOME/thegn/config.toml" <<EOF

[[pipeline.stages]]
name = "smoke"
agent = "claude"
prompt = "task {issue_number} on {branch} in {worktree}, artifact {artifact}"
EOF
# `dispatch put` is the roster's writer, and the v56 pipeline columns
# (stage/parent/session/artifact) ride it — there is no second verb. A parent id
# that names no row is refused BEFORE the insert, so a typo cannot leave a chunk
# row orphaned off the board.
check "dispatch put records the pipeline columns" \
  "'$SZ' dispatch put linear:SMOKE-1 '$R' claude --stage architect --session s1 --json | grep -q '\"stage\":\"architect\"'"
check "dispatch list shows the new row's stage" \
  "'$SZ' dispatch list | grep -q 'architect'"
check "dispatch put rejects a parent that does not exist" \
  "'$SZ' dispatch put linear:SMOKE-2 '$R' claude --parent 999999 >/dev/null 2>&1; [[ \$? -ne 0 ]]"

# --- run-completion contract (THE-76): wait / verify / the gated done --------
# The wait verbs answer from the local roster alone when the selection has
# nothing to wait on — they must not require (or contact) a daemon, so the
# error text is the roster's, never the no-daemon message. Row 1 (from above)
# is `queued`: not a live worker, so `--any` has nothing to wait on.
set +e
wany_out="$("$SZ" dispatch wait --any 2>&1)"
wany_rc=$?
wrow_out="$("$SZ" dispatch wait --row 999999 2>&1)"
wrow_rc=$?
v1_out="$("$SZ" dispatch verify 1 2>&1)"
v1_rc=$?
set -e
wany_ok=1
[[ $wany_rc -ne 0 ]] && grep -q 'nothing to wait on' <<<"$wany_out" || wany_ok=0
if grep -q 'no thegn pane daemon' <<<"$wany_out"; then wany_ok=0; fi
check "dispatch wait --any with nothing active exits non-zero without a daemon" \
  "[[ $wany_ok -eq 1 ]]"
wrow_ok=1
[[ $wrow_rc -ne 0 ]] && grep -q 999999 <<<"$wrow_out" || wrow_ok=0
check "dispatch wait --row 999999 exits non-zero naming the id" \
  "[[ $wrow_ok -eq 1 ]]"
v1_ok=1
[[ $v1_rc -eq 0 ]] && grep -q 'ok=yes' <<<"$v1_out" || v1_ok=0
check "dispatch verify on a row with no artifact reports ok and exits 0" \
  "[[ $v1_ok -eq 1 ]]"
check "dispatch put records an artifact pointer" \
  "'$SZ' dispatch put linear:SMOKE-3 '$R' claude --stage code --artifact .thegn/pipeline/SMOKE-7/missing/2.md --json | grep -q '\"artifact_path\":\".thegn/pipeline/SMOKE-7/missing/2.md\"'"
# A missing artifact is a retryable not-yet (exit 2, the `session wait`
# convention) naming the artifact; `set-status done` is refused for the same
# reason unless `--force` records it as forced — never invisibly.
set +e
v2_out="$("$SZ" dispatch verify 2 2>&1)"
v2_rc=$?
gate_out="$("$SZ" dispatch set-status 2 "done" 2>&1)"
gate_rc=$?
set -e
v2_ok=1
[[ $v2_rc -eq 2 ]] && grep -q 'does not exist' <<<"$v2_out" || v2_ok=0
grep -q 'SMOKE-7/missing/2.md' <<<"$v2_out" || v2_ok=0
check "dispatch verify on a missing artifact exits 2 naming it" \
  "[[ $v2_ok -eq 1 ]]"
gate_ok=1
[[ $gate_rc -ne 0 ]] && grep -q 'not verifiably finished' <<<"$gate_out" || gate_ok=0
check "set-status done is refused for a row with a missing artifact" \
  "[[ $gate_ok -eq 1 ]]"
check "set-status done --force overrides and says so" \
  "'$SZ' dispatch set-status 2 done --force | grep -q forced"
# Untracked: the artifact exists but git does not track it — the exact pilot
# failure ("session exit ≠ done") the gate exists to catch. Committing it
# flips the gate open; a forced completion stays visible in the output.
check "dispatch put records an untracked artifact row" \
  "'$SZ' dispatch put linear:SMOKE-4 '$R' claude --stage review --artifact .thegn/pipeline/SMOKE-7/untracked/3.md >/dev/null"
mkdir -p "$R/.thegn/pipeline/SMOKE-7/untracked"
echo handoff >"$R/.thegn/pipeline/SMOKE-7/untracked/3.md"
set +e
utr_out="$("$SZ" dispatch set-status 3 "done" 2>&1)"
utr_rc=$?
set -e
utr_ok=1
[[ $utr_rc -ne 0 ]] && grep -q 'does not track' <<<"$utr_out" || utr_ok=0
check "set-status done is refused while the artifact is untracked" \
  "[[ $utr_ok -eq 1 ]]"
git -C "$R" add .thegn/pipeline/SMOKE-7/untracked/3.md
git -C "$R" commit -q -m 'smoke: commit the artifact'
check "set-status done passes once the artifact is tracked" \
  "'$SZ' dispatch set-status 3 done | grep -q 'done'"
# `session open` shares the control-client connect path, so it degrades with
# the same clear no-daemon message rather than crashing.
set +e
sopen_out="$("$SZ" session open --agent claude --worktree "$R" 2>&1)"
sopen_rc=$?
set -e
sopen_ok=1
[[ $sopen_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$sopen_out" || sopen_ok=0
check "session open without a daemon exits 1 with a clear message" \
  "[[ $sopen_ok -eq 1 ]]"
# THE-76: `session close` shares that same connect path, `session list --live`
# degrades identically in both modes, and `session open`'s offline refusals
# (stage lookup, clap conflict, empty-prompt-with-headless) are answerable
# without a daemon at all — all checked daemon-free, same style as above.
set +e
sclose_out="$($SZ session close bogus 2>&1)"
sclose_rc=$?
sclose_json="$($SZ session close bogus --json 2>/dev/null)"
slive_out="$($SZ session list --live 2>&1)"
slive_rc=$?
slive_json="$($SZ session list --live --json 2>/dev/null)"
sstage_out="$($SZ session open --stage nosuchstage --issue linear:SMOKE-1 --worktree "$R" 2>&1)"
sstage_rc=$?
# Overlay form (`--stage` WITHOUT `--issue`, THE-83): a legal plain open, so
# with no daemon it must get as far as the offline stage-miss refusal (the
# same stage_or_bail the dispatch path uses) — never a clap error.
sstageoverlay_out="$($SZ session open --stage nosuchstage --prompt Y --worktree "$R" 2>&1)"
sstageoverlay_rc=$?
# Dispatch + explicit --prompt: the template owns the task, refused offline.
# (Stage 'smoke' is appended to the config below, before this runs.)
sstageprompt_out="$($SZ session open --stage smoke --issue linear:SMOKE-1 --prompt Y --worktree "$R" 2>&1)"
sstageprompt_rc=$?
sheadless_out="$($SZ session open --agent claude --worktree "$R" --headless 2>&1)"
sheadless_rc=$?
set -e
close_ok=1
[[ $sclose_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$sclose_out" || close_ok=0
check "session close without a daemon exits 1 with a clear message" \
  "[[ $close_ok -eq 1 ]]"
close_json_ok=1
grep -q 'no_daemon' <<<"$sclose_json" || close_json_ok=0
check "session close --json emits the no_daemon error object" \
  "[[ $close_json_ok -eq 1 ]]"
slive_ok=1
[[ $slive_rc -eq 1 ]] && grep -q 'no thegn pane daemon' <<<"$slive_out" || slive_ok=0
check "session list --live without a daemon exits 1 with a clear message" \
  "[[ $slive_ok -eq 1 ]]"
slive_json_ok=1
grep -q 'no_daemon' <<<"$slive_json" || slive_json_ok=0
check "session list --live --json emits the no_daemon error object" \
  "[[ $slive_json_ok -eq 1 ]]"
sstage_ok=1
[[ $sstage_rc -ne 0 ]] && grep -q 'nosuchstage' <<<"$sstage_out" || sstage_ok=0
check "session open --stage with an unknown stage fails offline naming it" \
  "[[ $sstage_ok -eq 1 ]]"
sstageoverlay_ok=1
[[ $sstageoverlay_rc -ne 0 ]] && grep -q 'nosuchstage' <<<"$sstageoverlay_out" || sstageoverlay_ok=0
check "session open --stage without --issue takes the overlay path (offline stage miss)" \
  "[[ $sstageoverlay_ok -eq 1 ]]"
sstageprompt_ok=1
[[ $sstageprompt_rc -ne 0 ]] && grep -q 'template owns the task' <<<"$sstageprompt_out" || sstageprompt_ok=0
check "session open --stage --issue refuses an explicit --prompt offline" \
  "[[ $sstageprompt_ok -eq 1 ]]"
sheadless_ok=1
[[ $sheadless_rc -ne 0 ]] && grep -q 'empty prompt' <<<"$sheadless_out" || sheadless_ok=0
check "session open --headless with no prompt is refused" \
  "[[ $sheadless_ok -eq 1 ]]"

# THE-86: `--resume-work` answers its row checks offline, before any daemon
# contact — an unknown row is named outright (never the no-daemon message),
# and a plain (non-`--stage`) row is refused as not a pipeline row. Both
# daemon-free, same style as above.
set +e
sresume_out="$($SZ session open --resume-work 999999 2>&1)"
sresume_rc=$?
sresume_row="$($SZ dispatch put linear:SMOKE-5 "$R" claude 2>/dev/null | sed -n 's/^dispatch \([0-9]*\) .*/\1/p')"
sresumeplain_out="$($SZ session open --resume-work "$sresume_row" 2>&1)"
sresumeplain_rc=$?
set -e
sresume_ok=1
[[ $sresume_rc -ne 0 ]] && grep -q 999999 <<<"$sresume_out" || sresume_ok=0
if grep -q 'no thegn pane daemon' <<<"$sresume_out"; then sresume_ok=0; fi
check "session open --resume-work refuses an unknown row offline, naming it" \
  "[[ $sresume_ok -eq 1 ]]"
sresumeplain_ok=1
[[ $sresumeplain_rc -ne 0 ]] && grep -q 'not a pipeline row' <<<"$sresumeplain_out" || sresumeplain_ok=0
check "session open --resume-work refuses a non-pipeline row" \
  "[[ $sresumeplain_ok -eq 1 ]]"
# A row the Lead already closed is not a resume point (THE-86 review): a done
# pipeline row is refused daemon-free, naming the status.
set +e
sdone_row="$($SZ dispatch put linear:SMOKE-5 "$R" claude --stage code 2>/dev/null | sed -n 's/^dispatch \([0-9]*\) .*/\1/p')"
# `done` is passed via a variable: a literal `done` inside $() reads as a
# shell keyword to shellcheck (SC1010).
sdone_status="done"
sresumedone_out="$($SZ dispatch set-status "$sdone_row" "$sdone_status" >/dev/null 2>&1 && $SZ session open --resume-work "$sdone_row" 2>&1)"
sresumedone_rc=$?
set -e
sresumedone_ok=1
[[ $sresumedone_rc -ne 0 ]] && grep -q 'is done' <<<"$sresumedone_out" || sresumedone_ok=0
[[ $sresumedone_rc -ne 0 ]] && grep -q 'not a resume point' <<<"$sresumedone_out" || sresumedone_ok=0
check "session open --resume-work refuses a done row" \
  "[[ $sresumedone_ok -eq 1 ]]"
# The tracker doors honestly report an unconfigured tracker (the AI-free shell:
# the verb exists, the provider simply is not wired) rather than pretending.
check "issue list --status errors with no tracker configured" \
  "'$SZ' issue list --status todo --limit 3 --json >/dev/null 2>&1; [[ \$? -ne 0 ]]"
check "wt new --from-issue errors with no tracker configured" \
  "'$SZ' wt new --from-issue linear:NONE-1 --repo '$R' >/dev/null 2>&1; [[ \$? -ne 0 ]]"

# --- chunk file-scope gate (THE-86): put --chunk / the scope display ---------
# Daemon-free: the gate is roster + chunk-file logic over the local DB. A
# second worktree of the smoke repo holds the chunk files and the rows dispatch
# against it, so every scope read below provably comes from the row's OWN
# recorded worktree, not from the CLI's cwd.
CWT="$TMP/wt/chunk-scope"
git -C "$R" worktree add -q -b smoke-chunks "$CWT" main
mkdir -p "$CWT/.thegn/pipeline/SMOKE-7/code"
cat >"$CWT/.thegn/pipeline/SMOKE-7/code/chunk-1.md" <<'EOF'
---
files:
  - crates/thegn-core/src/pipeline_run.rs
---
# chunk 1
EOF
cat >"$CWT/.thegn/pipeline/SMOKE-7/code/chunk-2.md" <<'EOF'
---
files: [crates/thegn-core/src/pipeline_run.rs]
---
# chunk 2 — shares chunk-1's file with no overlaps: blessing
EOF
cat >"$CWT/.thegn/pipeline/SMOKE-7/code/chunk-3.md" <<'EOF'
---
files:
  - crates/thegn-core/src/db.rs
after: [chunk-1]
---
# chunk 3 — waits for chunk-1
EOF
# shellcheck disable=SC2034 # read by the `check` bodies below, which run under `eval`
CA_JSON="$($SZ dispatch put linear:SMOKE-6 "$CWT" claude --chunk .thegn/pipeline/SMOKE-7/code/chunk-1.md --json)"
check "dispatch put --chunk records the chunk_path" \
  "printf '%s' \"\$CA_JSON\" | grep -q '\"chunk_path\":\".thegn/pipeline/SMOKE-7/code/chunk-1.md\"'"
CA_ROW="$(printf '%s' "$CA_JSON" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')"
check "dispatch list --json carries the parsed chunk_files" \
  "'$SZ' dispatch list --json | grep -q '\"chunk_files\"' && '$SZ' dispatch list --json | grep -q 'pipeline_run.rs'"
set +e
cgate_out="$($SZ dispatch put linear:SMOKE-6 "$CWT" claude --chunk .thegn/pipeline/SMOKE-7/code/chunk-2.md 2>&1)"
cgate_rc=$?
set -e
cgate_ok=1
[[ $cgate_rc -ne 0 ]] && grep -q 'chunk scope gate refused' <<<"$cgate_out" || cgate_ok=0
grep -q 'collides with' <<<"$cgate_out" || cgate_ok=0
grep -q 'active row' <<<"$cgate_out" || cgate_ok=0
grep -q -- '--force' <<<"$cgate_out" || cgate_ok=0
check "dispatch put --chunk refuses an overlapping ACTIVE sibling" \
  "[[ $cgate_ok -eq 1 ]]"
check "dispatch put --chunk --force overrides the gate and says so" \
  "'$SZ' dispatch put linear:SMOKE-6 '$CWT' claude --chunk .thegn/pipeline/SMOKE-7/code/chunk-2.md --force | grep -q forced"
set +e
cafter_out="$($SZ dispatch put linear:SMOKE-6 "$CWT" claude --chunk .thegn/pipeline/SMOKE-7/code/chunk-3.md 2>&1)"
cafter_rc=$?
set -e
cafter_ok=1
[[ $cafter_rc -ne 0 ]] && grep -q 'after chunk-1 is not done' <<<"$cafter_out" || cafter_ok=0
check "an after: chunk whose prerequisite is not done is refused" \
  "[[ $cafter_ok -eq 1 ]]"
# Finishing chunk-1 (done) flips the after-gate open — the normal pipeline
# order. Its row has no artifact pointer, so the done gate passes by construction.
check "a done prerequisite satisfies the after gate" \
  "'$SZ' dispatch set-status '$CA_ROW' done >/dev/null && '$SZ' dispatch put linear:SMOKE-6 '$CWT' claude --chunk .thegn/pipeline/SMOKE-7/code/chunk-3.md | grep -q 'queued'"

# Daemon lifecycle: spawn on an isolated socket, open a marker session over
# the unix socket, see it in `session list` and its output in `snapshot`,
# then stop it and verify the registry row + socket are gone.
if command -v curl >/dev/null 2>&1; then
  DSOCK="$TMP/d.sock"
  "$SZ" daemon --socket "$DSOCK" &
  DPID=$!
  for _ in $(seq 1 40); do
    [[ -S $DSOCK ]] && break
    sleep 0.1
  done
  check "daemon binds its control socket" "[[ -S '$DSOCK' ]]"
  curl -s --unix-socket "$DSOCK" -X POST http://d/v1/sessions \
    -H 'content-type: application/json' \
    -d '{"argv":["/bin/sh","-c","echo smoke-marker; sleep 30"],"rows":24,"cols":80}' >/dev/null
  sleep 0.5
  slist_ok=1
  "$SZ" session list | grep -Eq 'sh|echo' || slist_ok=0
  check "session list shows the daemon-owned session" "[[ $slist_ok -eq 1 ]]"
  sid="$("$SZ" session list --json | sed -n 's/.*"id": "\([a-f0-9]*\)".*/\1/p' | head -1)"
  snap_ok=1
  "$SZ" session snapshot --session "$sid" | grep -aq smoke-marker || snap_ok=0
  check "snapshot carries the detached session's output" "[[ $snap_ok -eq 1 ]]"

  # Orchestration control routes over the same socket (THE-57): the dispatch
  # roster records and re-statuses a row, and `worktrees.create` spins up a
  # worktree from a branch — both real ControlApi paths, exercised end-to-end.
  curl -s --unix-socket "$DSOCK" -X POST http://d/v1/dispatches \
    -H 'content-type: application/json' \
    -d '{"issue_id":"smoke:1","worktree_path":"/wt/smoke","agent_name":"claude"}' >/dev/null
  disp_json="$(curl -s --unix-socket "$DSOCK" http://d/v1/dispatches)"
  check "dispatches.put + dispatches.list round-trip over HTTP" \
    "grep -q 'smoke:1' <<<'$disp_json' && grep -q '\"queued\"' <<<'$disp_json'"
  wc_json="$(curl -s --unix-socket "$DSOCK" -X POST http://d/v1/worktrees \
    -H 'content-type: application/json' \
    -d "{\"repo\":\"$R\",\"branch\":\"smoke-create\"}")"
  check "worktrees.create makes a worktree over HTTP" \
    "grep -q '\"branch\":\"smoke-create\"' <<<'$wc_json' && \
     git -C '$R' worktree list --porcelain | grep -q 'smoke-create'"

  kill "$DPID" 2>/dev/null || true
  wait "$DPID" 2>/dev/null || true
  for _ in $(seq 1 20); do
    [[ ! -S $DSOCK ]] && break
    sleep 0.1
  done
  check "daemon cleanup unlinks the socket" "[[ ! -S '$DSOCK' ]]"
  if command -v sqlite3 >/dev/null 2>&1; then
    rows="$(sqlite3 "$XDG_STATE_HOME/thegn/thegn.db" 'SELECT count(*) FROM daemons' 2>/dev/null || echo 0)"
    check "daemon cleanup removes its registry row" "[[ '$rows' -eq 0 ]]"
  fi
else
  echo "  skip daemon lifecycle (curl not on PATH)"
fi

# CLI verbs never spawn a daemon as a side effect — only PANE spawns lazily
# ensure one (the default-on [daemon] routes panes, not verbs). Every verb
# above ran daemon-less; no socket may exist on either default path.
check "CLI verbs never spawn a daemon" \
  "[[ ! -S \"$XDG_RUNTIME_DIR/thegn/daemon.sock\" && ! -S \"$XDG_STATE_HOME/thegn/run/daemon.sock\" ]]"

# Explicit close kills: DELETE on a session ends its child (the close-a-pane
# contract at the API level). The row itself lingers briefly — `daemon/
# tombstone.rs` keeps a corpse on purpose so a supervisor can read a session's
# result without racing the moment of exit — so "killed" is the row being
# *marked finished*, not the row vanishing.
if command -v curl >/dev/null 2>&1; then
  DSOCK2="$TMP/d2.sock"
  "$SZ" daemon --socket "$DSOCK2" &
  D2PID=$!
  for _ in $(seq 1 40); do
    [[ -S $DSOCK2 ]] && break
    sleep 0.1
  done
  curl -s --unix-socket "$DSOCK2" -X POST http://d/v1/sessions \
    -H 'content-type: application/json' \
    -d '{"argv":["/bin/sh","-c","sleep 30"],"rows":24,"cols":80}' >/dev/null
  sleep 0.3
  ksid="$("$SZ" session list --json | sed -n 's/.*"id": "\([a-f0-9]*\)".*/\1/p' | head -1)"
  kill_ok=1
  # An empty id would make every grep below match vacuously and "pass".
  [[ -n $ksid ]] || kill_ok=0
  "$SZ" session list --json 2>/dev/null | grep -q '"exited_at_ms"' && kill_ok=0
  curl -s --unix-socket "$DSOCK2" -X DELETE "http://d/v1/sessions/$ksid" >/dev/null
  sleep 0.5
  "$SZ" session list --json 2>/dev/null | grep -q '"exited_at_ms"' || kill_ok=0
  check "DELETE kills the session (explicit close = kill)" "[[ $kill_ok -eq 1 ]]"
  kill "$D2PID" 2>/dev/null || true
  wait "$D2PID" 2>/dev/null || true
else
  echo "  skip close-kill check (curl not on PATH)"
fi

# --- one-time superzej -> thegn migration -----------------------------------
# Seed old-brand state/config/app-home in a fresh throwaway HOME, run any CLI
# verb, and assert the startup migration renamed everything (marker included).
MIG="$(mktemp -d)"
mkdir -p "$MIG/.local/state/superzej" "$MIG/.config/superzej" "$MIG/.superzej/worktrees"
printf 'stale' >"$MIG/.local/state/superzej/superzej.db"
printf 'worktrees_dir = "%s/wt"\n' "$MIG" >"$MIG/.config/superzej/config.toml"
env HOME="$MIG" XDG_CONFIG_HOME="$MIG/.config" XDG_STATE_HOME="$MIG/.local/state" \
  "$SZ" repos >/dev/null 2>&1 || true
check "migration moved the state dir + db" \
  "[[ -f '$MIG/.local/state/thegn/thegn.db' && ! -e '$MIG/.local/state/superzej' ]]"
check "migration moved the config dir" \
  "[[ -f '$MIG/.config/thegn/config.toml' && ! -e '$MIG/.config/superzej' ]]"
check "migration moved the app home" \
  "[[ -d '$MIG/.thegn/worktrees' && ! -e '$MIG/.superzej' ]]"
check "migration wrote its forensics marker" \
  "[[ -f '$MIG/.thegn/.migrated-from-superzej' ]]"
check "migration honors THEGN_NO_MIGRATE" \
  "mkdir -p '$MIG/.config/superzej' && env HOME='$MIG' XDG_CONFIG_HOME='$MIG/.config' XDG_STATE_HOME='$MIG/.local/state' THEGN_NO_MIGRATE=1 '$SZ' repos >/dev/null 2>&1 || true; [[ -d '$MIG/.config/superzej' ]]"
rm -rf "$MIG"

# --- release channels (stable vs dev) -------------------------------------
# The smoke run above is dev-channel; here we assert the STABLE channel refuses
# the experimental verbs and clamps the experimental config toggles.
echo "release channels:"
check "stable channel refuses an experimental verb (host)" \
  "! env THEGN_CHANNEL=stable '$SZ' host list >/dev/null 2>&1"
check "the refusal names the dev channel" \
  "{ env THEGN_CHANNEL=stable '$SZ' host list 2>&1 || true; } | grep -q 'dev-channel feature'"
check "dev channel allows the same verb" \
  "env THEGN_CHANNEL=dev '$SZ' host list >/dev/null 2>&1"
check "doctor reports the stable channel + disabled remote" \
  "env THEGN_CHANNEL=stable '$SZ' doctor --json | grep -q '\"channel\": \"stable\"' && env THEGN_CHANNEL=stable '$SZ' doctor --json | grep -A8 '\"features\"' | grep -q '\"remote\": false'"
check "doctor reports the dev channel + enabled remote" \
  "env THEGN_CHANNEL=dev '$SZ' doctor --json | grep -A8 '\"features\"' | grep -q '\"remote\": true'"
# The provider-seams registry: every configured seam reports a probe, and
# the text twin prints the same section.
check "doctor --json lists providers with seam/id/availability" \
  "'$SZ' doctor --json | grep -q '\"seam\": \"' && '$SZ' doctor --json | grep -q '\"availability\": {'"
check "doctor reports a Providers section" \
  "'$SZ' doctor | grep -q '^Providers'"
check "plugin list is empty-clean and check passes with no plugins" \
  "'$SZ' plugin list | grep -q 'no plugins configured' && '$SZ' plugin check | grep -q 'plugins: ok'"
CHCFG="$TMP/channel.toml"
printf '[observe]\nenabled = true\n' >"$CHCFG"
check "stable clamps experimental toggles off" \
  "env THEGN_CHANNEL=stable '$SZ' --config '$CHCFG' config show | grep -A1 '^\[observe\]' | grep -q 'enabled = false'"
check "dev honours the same toggles" \
  "env THEGN_CHANNEL=dev '$SZ' --config '$CHCFG' config show | grep -A1 '^\[observe\]' | grep -q 'enabled = true'"

echo
if [[ $fail -eq 0 ]]; then
  printf '\033[32mall smoke checks passed\033[0m\n'
else
  printf '\033[31msmoke test FAILED\033[0m\n'
  exit 1
fi
