#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

native_out="$("$repo"/install.sh --dry-run --native "$tmp/bin")"
[[ $native_out == *"mode: native"* ]] || {
  echo "native dry-run did not report native mode" >&2
  echo "$native_out" >&2
  exit 1
}
[[ $native_out == *"$tmp/bin/superzej -> $repo/target/release/szhost"* ]] || {
  echo "native dry-run did not plan superzej -> szhost" >&2
  echo "$native_out" >&2
  exit 1
}
[[ $native_out == *"$tmp/bin/sj -> $repo/target/release/szhost"* ]] || {
  echo "native dry-run did not plan sj -> szhost" >&2
  echo "$native_out" >&2
  exit 1
}
[[ $native_out == *"$tmp/bin/szhost -> $repo/target/release/szhost"* ]] || {
  echo "native dry-run did not plan szhost alias" >&2
  echo "$native_out" >&2
  exit 1
}
[[ $native_out != *"building WASM plugins"* ]] || {
  echo "native dry-run should not build zellij WASM plugins" >&2
  echo "$native_out" >&2
  exit 1
}
[[ $native_out == *"legacy WASM plugin symlinks under $HOME/.local/share/superzej will be removed"* ]] || {
  echo "native dry-run did not plan legacy WASM symlink cleanup" >&2
  echo "$native_out" >&2
  exit 1
}

zellij_out="$("$repo"/install.sh --dry-run --zellij "$tmp/bin")"
[[ $zellij_out == *"mode: zellij"* ]] || {
  echo "zellij dry-run did not report zellij mode" >&2
  echo "$zellij_out" >&2
  exit 1
}
[[ $zellij_out == *"$tmp/bin/superzej -> $repo/target/release/superzej"* ]] || {
  echo "zellij dry-run did not plan superzej -> old cli" >&2
  echo "$zellij_out" >&2
  exit 1
}
[[ $zellij_out == *"$HOME/.local/share/superzej/{sidebar,panel,tabbar,statusbar}.wasm"* ]] || {
  echo "zellij dry-run did not report plugin install paths" >&2
  echo "$zellij_out" >&2
  exit 1
}

echo "install plan checks passed"
