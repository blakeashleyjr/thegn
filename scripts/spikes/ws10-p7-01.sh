#!/usr/bin/env bash
# WS10-P7-01 — source-level reading aid for the E2B / OpenSandbox / Runloop / Modal
# protocol record. Usage: ws10-p7-01.sh OUTDIR
# Shallow-clones each public repo into its OWN fresh directory under OUTDIR (downloaded
# source is untrusted data: nothing in it is built or executed here), records the commit,
# lists the spec files, and greps starting-point lines (stdin / pty / resize / headers)
# into the evidence file. The DECISIONS (OpenSandbox PTY? which SaaS vendor?) are a human
# reading step; the only computed hint is a grep count, labelled as such.
# Repo URLs are overridable: E2B_REPO, OPENSANDBOX_REPO, RUNLOOP_REPOS (space separated),
# MODAL_REPO. CONFIRM the URLs in the procedure page before trusting a clone failure.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS10-P7-01
sp_init "$item" "${1:-}"
sp_need git grep find
src="$SP_OUT/$item/src"
mkdir -p "$src"
sp_on_exit "rm -rf '$src'"

E2B_REPO="${E2B_REPO:-https://github.com/e2b-dev/runtime}" # name from the issue; e2b-dev/infra is the likely alternative
OPENSANDBOX_REPO="${OPENSANDBOX_REPO:-https://github.com/opensandbox-group/OpenSandbox}"
RUNLOOP_REPOS="${RUNLOOP_REPOS:-https://github.com/runloopai/api-client-python https://github.com/runloopai/rl-cli}"
MODAL_REPO="${MODAL_REPO:-https://github.com/modal-labs/modal-client}"

clone() { # clone NAME URL -> dir on stdout, or empty
  local name="$1" url="$2" d="$src/$1"
  rm -rf "$d"
  mkdir -p "$d"
  if git -c core.hooksPath=/dev/null -c protocol.file.allow=never clone -q --depth 1 "$url" "$d" 2>"$SP_OUT/$item/clone-$name.err"; then
    sp_kv "repo_$name" "$url @ $(git -C "$d" rev-parse HEAD)"
    printf '%s' "$d"
  else
    sp_kv "repo_$name" "$url: CLONE FAILED (see clone-$name.err) — fix the URL, record the actual repo"
  fi
}

hits() { # hits NAME DIR PATTERN [globs...]
  local name="$1" dir="$2" pat="$3"
  shift 3
  sp_note ""
  sp_note "## $name: grep -rnEi '$pat' ${*:-.}"
  (cd "$dir" && grep -rnEi --exclude-dir=.git --exclude-dir=node_modules -e "$pat" "${@:-.}" 2>/dev/null | head -n 60) | sp_redact >>"$SP_EVID" || true
}

d="$(clone e2b "$E2B_REPO")"
if [ -n "$d" ]; then
  sp_note ""
  sp_note "## e2b: candidate spec files"
  (cd "$d" && find . \( -name '*.proto' -o -name 'openapi*.y*ml' -o -name 'openapi*.json' -o -name 'swagger*' \) -not -path './.git/*' | head -n 40) >>"$SP_EVID"
  hits e2b-process "$d" 'StartRequest|ProcessInput|StreamInput|resize|pty|SendSignal|connect-protocol|Connect-' --include=*.proto
  hits e2b-auth "$d" 'X-API-KEY|Authorization|x-api-key|allowOut|denyOut|metadata' --include=*.y*ml --include=*.json
fi

d="$(clone opensandbox "$OPENSANDBOX_REPO")"
if [ -n "$d" ]; then
  sp_note ""
  sp_note "## opensandbox: spec files named by the issue"
  for f in specs/sandbox-lifecycle.yml specs/execd-api.yaml specs/egress-api.yaml; do
    if [ -f "$d/$f" ]; then sp_note "present: $f"; else sp_note "MISSING: $f (find the current names)"; fi
  done
  hits opensandbox-execd "$d" 'stdin|pty|resize|websocket|OPEN-SANDBOX-API-KEY|/command|/session' specs
  stdin_hits="$(cd "$d" && grep -rniE 'stdin|pty|websocket' specs/execd-api.yaml 2>/dev/null | wc -l | tr -d ' ')"
  sp_kv opensandbox_execd_stdin_pty_websocket_grep_count "$stdin_hits (HINT ONLY: 0 suggests no bidirectional stdin; read the spec before deciding pty=false)"
fi

i=0
for url in $RUNLOOP_REPOS; do
  i=$((i + 1))
  d="$(clone "runloop$i" "$url")"
  [ -n "$d" ] && hits "runloop$i" "$d" 'tunnel|pty|websocket|/v1/devboxes|ssh|metadata|label' .
done

d="$(clone modal "$MODAL_REPO")"
[ -n "$d" ] && hits modal "$d" 'sandbox.*exec|ContainerExec|pty|stdin|exec_with_pty' .

sp_verdict "OpenSandbox execd without bidirectional stdin => pty=false; Runloop PTY not reproducible from SDK source => SaaS reference plugin is Modal (Modal without PTY => Runloop pty=false); unsettled facts stay 'unverified'" \
  "reading aid written; both decisions are a human step from the evidence above (see the procedure page for what to read)"
