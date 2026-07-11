#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

out="$("$repo"/install.sh --dry-run "$tmp/bin")"
[[ $out == *"$tmp/bin/thegn -> $repo/target/release/thegn"* ]] || {
  echo "dry-run did not plan the thegn symlink" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"$tmp/bin/tg-tui wrapper -> $repo/target/release/thegn (current terminal)"* ]] || {
  echo "dry-run did not plan tg-tui current-terminal wrapper" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"$tmp/bin/tg wrapper -> alacritty --config-file $repo/config/alacritty.toml -e $tmp/bin/tg-tui"* ]] || {
  echo "dry-run did not plan tg dedicated alacritty wrapper" >&2
  echo "$out" >&2
  exit 1
}
# The native installer must never build or reference zellij WASM plugins.
[[ $out != *"WASM"* && $out != *"plugin"* && $out != *"zellij"* ]] || {
  echo "dry-run should not mention zellij/WASM plugins" >&2
  echo "$out" >&2
  exit 1
}

fakebin="$tmp/fakebin"
mkdir -p "$fakebin"
cat >"$fakebin/cargo" <<'EOF'
#!/usr/bin/env sh
exit 0
EOF
chmod 0755 "$fakebin/cargo"
cat >"$fakebin/delta" <<'EOF'
#!/usr/bin/env sh
exit 0
EOF
chmod 0755 "$fakebin/delta"
cat >"$fakebin/alacritty" <<'EOF'
#!/usr/bin/env sh
printf '%s\n' "$@" >"${TG_ALACRITTY_LOG:?}"
EOF
chmod 0755 "$fakebin/alacritty"

install_out="$(PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" "$repo/install.sh" "$tmp/bin")"
[[ -L $tmp/bin/thegn && $(readlink "$tmp/bin/thegn") == "$repo/target/release/thegn" ]] || {
  echo "install did not symlink thegn to the release binary" >&2
  echo "$install_out" >&2
  exit 1
}
[[ -x $tmp/bin/tg && -x $tmp/bin/tg-tui ]] || {
  echo "install did not create executable tg and tg-tui wrappers" >&2
  echo "$install_out" >&2
  exit 1
}
[[ $(<"$tmp/bin/tg-tui") == *"exec $repo/target/release/thegn"* ]] || {
  echo "tg-tui should exec thegn directly in the current terminal" >&2
  sed -n '1,120p' "$tmp/bin/tg-tui" >&2
  exit 1
}
[[ $(<"$tmp/bin/tg") == *"exec alacritty --config-file $repo/config/alacritty.toml -e $tmp/bin/tg-tui"* ]] || {
  echo "tg should launch the dedicated alacritty profile" >&2
  sed -n '1,120p' "$tmp/bin/tg" >&2
  exit 1
}
TG_ALACRITTY_LOG="$tmp/alacritty.args" PATH="$fakebin:$PATH" "$tmp/bin/tg"
alacritty_args="$(<"$tmp/alacritty.args")"
[[ $alacritty_args == *$'--config-file\n'"$repo/config/alacritty.toml"*$'\n-e\n'"$tmp/bin/tg-tui"* ]] || {
  echo "tg did not invoke alacritty with the bundled config and tg-tui" >&2
  printf '%s\n' "$alacritty_args" >&2
  exit 1
}

echo "install plan checks passed"
