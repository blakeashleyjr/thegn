#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

out="$("$repo"/install.sh --dry-run "$tmp/bin")"
[[ $out == *"$tmp/bin/superzej -> $repo/target/release/szhost"* ]] || {
  echo "dry-run did not plan superzej -> szhost" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"$tmp/bin/sj -> $repo/target/release/szhost"* ]] || {
  echo "dry-run did not plan sj -> szhost" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"$tmp/bin/szhost -> $repo/target/release/szhost"* ]] || {
  echo "dry-run did not plan szhost alias" >&2
  echo "$out" >&2
  exit 1
}
# The native installer must never build or reference zellij WASM plugins.
[[ $out != *"WASM"* && $out != *"plugin"* && $out != *"zellij"* ]] || {
  echo "dry-run should not mention zellij/WASM plugins" >&2
  echo "$out" >&2
  exit 1
}

echo "install plan checks passed"
