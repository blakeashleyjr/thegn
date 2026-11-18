#!/usr/bin/env bash
# WS08-P0-02 — Hetzner VPS lifecycle on a LIVE, FUNDED account. COSTS MONEY
# (a few cents to a few euros) and creates real billable servers.
# Usage: HCLOUD_TOKEN=... [RUN_CARGO=1] [STEP3_PASS=1] [HETZNER_DESTROY_LEFTOVERS=1] \
#        ws08-p0-02.sh OUTDIR
#   HCLOUD_TOKEN   least-scope token for a DEDICATED project with a spend alert.
#                  Read from the environment only; the script passes it to curl
#                  through a 0600 config file, never on argv.
#   RUN_CARGO=1    run step 1: `cargo test -p thegn-svc --test vps_live -- --ignored
#                  --nocapture live_hetzner` from the repo (compiles; once, quiet machine)
#   STEP3_PASS=1   you ran step 3 by hand (thegn env provision / pane / deprovision;
#                  ledger clean) and it passed
#   HETZNER_DESTROY_LEFTOVERS=1  delete servers that exist now but not at the start
# The script snapshots the project's server list before and after (step 6, the leak
# proof) and computes the decision rule for steps 1, 3 and 6. Steps 2 and 4 are manual.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
# shellcheck disable=SC1091 # the pre-commit shellcheck runs without -x
. "$here/lib.sh"

item=WS08-P0-02
sp_init "$item" "${1:-}"
sp_need curl jq
sp_need_env HCLOUD_TOKEN
api="https://api.hetzner.cloud/v1"

sec="$SP_OUT/$item/.secrets"
mkdir -p "$sec"
chmod 700 "$sec"
sp_on_exit "rm -rf '$sec'"
(
  umask 077
  printf 'header = "Authorization: Bearer %s"\n' "$HCLOUD_TOKEN" >"$sec/auth.curl"
)
hc() { curl -sS -m 60 -K "$sec/auth.curl" "$@"; }

list_ids() { hc "$api/servers?per_page=50" | jq -r '.servers[] | "\(.id)\t\(.name)\t\(.created)\t\(.server_type.name)\t\(.datacenter.location.name)"' | sort; }

start_epoch="$(date +%s)"
list_ids >"$SP_OUT/$item/servers-before.tsv" || sp_die "cannot list servers (bad token or no network)"
sp_kv servers_before "$(wc -l <"$SP_OUT/$item/servers-before.tsv" | tr -d ' ')"
sp_kv thegn_commit "$(git -C "$here" rev-parse HEAD 2>/dev/null || echo unknown)"
price="$(hc "$api/server_types?name=cx23" | jq -r '.server_types[0].prices[0] | "\(.location) hourly_gross=\(.price_hourly.gross)"' 2>/dev/null || echo unknown)"
sp_kv cx23_price "$price (default type cx23, location fsn1; cx22 is deprecated)"

step1="NOT RUN (RUN_CARGO unset)"
if [ "${RUN_CARGO:-0}" = 1 ]; then
  sp_need cargo
  root="$(git -C "$here" rev-parse --show-toplevel)"
  rc=0
  (cd "$root" && HCLOUD_TOKEN="$HCLOUD_TOKEN" cargo test -p thegn-svc --test vps_live -- --ignored --nocapture live_hetzner) \
    >"$SP_OUT/$item/step1.log" 2>&1 || rc=$?
  # redact public IPv4 addresses before anything is kept or committed
  sed -E 's/[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}/<IP>/g' "$SP_OUT/$item/step1.log" >"$SP_OUT/$item/step1.redacted.log"
  rm -f "$SP_OUT/$item/step1.log"
  if [ "$rc" -eq 0 ]; then step1=PASS; else step1="FAIL(rc=$rc)"; fi
fi
sp_kv step1_live_hetzner_test "$step1"

cat >>"$SP_EVID" <<'EOF'
MANUAL steps (record each result in docs/audits/provider-live-2026-11/hetzner.md):
  2. files round trip (ProviderFiles) against a live instance - via step 3 or one assertion
  3. thegn env provision -> pane command -> thegn env deprovision; server gone; ledger under
     $XDG_STATE_HOME/thegn/vps clean (then re-run with STEP3_PASS=1 to compute the verdict)
  4. orphan path: SIGKILL thegn between the cloud POST and ledger finalisation, then let
     vps_reaper run (orphan age 20 min, creating-stale 10 min, tick 300 s); OR record
     "not exercised: no crash hook" and open a fix-forward item
EOF

# step 6: leak proof
list_ids >"$SP_OUT/$item/servers-after.tsv"
comm -13 "$SP_OUT/$item/servers-before.tsv" "$SP_OUT/$item/servers-after.tsv" >"$SP_OUT/$item/servers-leftover.tsv" || true
nleft="$(wc -l <"$SP_OUT/$item/servers-leftover.tsv" | tr -d ' ')"
sp_kv servers_after "$(wc -l <"$SP_OUT/$item/servers-after.tsv" | tr -d ' ')"
sp_kv leftover_new_servers "$nleft"
if [ "$nleft" -gt 0 ]; then
  sp_note "LEFTOVER SERVERS (id, name, created, type, location):"
  cat "$SP_OUT/$item/servers-leftover.tsv" >>"$SP_EVID"
  if [ "${HETZNER_DESTROY_LEFTOVERS:-0}" = 1 ]; then
    while IFS="$(printf '\t')" read -r sid _rest; do
      hc -X DELETE "$api/servers/$sid" -o /dev/null -w "deleted $sid: %{http_code}\n" >>"$SP_EVID" || true
    done <"$SP_OUT/$item/servers-leftover.tsv"
    sp_note "destroyed leftovers; re-run the list to confirm"
  else
    sp_note "NOT destroyed (HETZNER_DESTROY_LEFTOVERS unset): delete them in the console. Orphans from step 4 may still be awaiting the reaper."
  fi
fi
sp_kv elapsed_s "$(($(date +%s) - start_epoch))"

step6=PASS
[ "$nleft" -eq 0 ] || step6="FAIL($nleft left)"
step3="NOT CONFIRMED"
[ "${STEP3_PASS:-0}" = 1 ] && step3=PASS
if [ "$step1" = PASS ] && [ "$step3" = PASS ] && [ "$step6" = PASS ]; then
  v="FLIP live_unverified for Hetzner (steps 1, 3, 6 pass). List steps 2/4 gaps in the evidence file and a follow-up."
else
  v="DO NOT FLIP: step1=$step1 step3=$step3 step6=$step6. File one fix-forward item per failure; the kind stays experimental."
fi
sp_verdict "steps 1, 3 and 6 pass => flip live_unverified; any of them fails => do not flip, file fix-forward items; 2/4 not exercised => still flip but list the gap" "$v"
