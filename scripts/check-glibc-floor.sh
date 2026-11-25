#!/usr/bin/env bash
# check-glibc-floor.sh <binary> [max-glibc] — fail if <binary> needs a glibc
# symbol version newer than max-glibc (default 2.28).
set -euo pipefail

bin="${1:?usage: check-glibc-floor.sh <binary> [max-glibc]}"
max="${2:-2.28}"
[[ -f $bin ]] || {
  echo "check-glibc-floor: no such file: $bin" >&2
  exit 2
}

versions="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9][0-9.]*' | sort -uV || true)"
if [[ -z $versions ]]; then
  echo "check-glibc-floor: $bin needs no versioned glibc symbols"
  exit 0
fi
newest="$(tail -n1 <<<"$versions")"
newest="${newest#GLIBC_}"
highest="$(printf '%s\n%s\n' "$newest" "$max" | sort -V | tail -n1)"
if [[ $highest != "$max" ]]; then
  echo "check-glibc-floor: $bin needs GLIBC_$newest, newer than the $max floor" >&2
  exit 1
fi
echo "check-glibc-floor: $bin needs GLIBC_$newest (floor $max)"
