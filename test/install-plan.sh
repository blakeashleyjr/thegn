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
[[ $out == *"$tmp/bin/tg wrapper -> $tmp/bin/tg-tui (current terminal); tg -s|--standalone -> alacritty --config-file $repo/config/alacritty.toml -e $tmp/bin/tg-tui"* ]] || {
  echo "dry-run did not plan tg current-terminal + alacritty standalone wrapper" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"app-launcher entry (Exec=$tmp/bin/tg --standalone, Icon=thegn)"* ]] || {
  echo "dry-run did not plan the desktop entry with Exec=tg --standalone / Icon=thegn" >&2
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

install_out="$(PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" "$repo/install.sh" "$tmp/bin")"
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
# tg with no standalone flag execs tg-tui directly (current terminal), never alacritty.
[[ $(<"$tmp/bin/tg") == *"exec $tmp/bin/tg-tui"* ]] || {
  echo "tg should exec tg-tui directly in the current terminal by default" >&2
  sed -n '1,120p' "$tmp/bin/tg" >&2
  exit 1
}
# tg -s / --standalone launches the dedicated alacritty profile.
[[ $(<"$tmp/bin/tg") == *"exec alacritty --config-file $repo/config/alacritty.toml -e $tmp/bin/tg-tui"* ]] || {
  echo "tg --standalone should launch the dedicated alacritty profile" >&2
  sed -n '1,120p' "$tmp/bin/tg" >&2
  exit 1
}
TG_ALACRITTY_LOG="$tmp/alacritty.args" PATH="$fakebin:$PATH" "$tmp/bin/tg" --standalone
alacritty_args="$(<"$tmp/alacritty.args")"
[[ $alacritty_args == *$'--config-file\n'"$repo/config/alacritty.toml"$'\n-e\n'"$tmp/bin/tg-tui"* ]] || {
  echo "tg --standalone did not invoke alacritty with the bundled config and tg-tui" >&2
  printf '%s\n' "$alacritty_args" >&2
  exit 1
}

# The desktop entry + owl icon must be installed under XDG_DATA_HOME.
desktop="$tmp/data/applications/thegn.desktop"
icon="$tmp/data/icons/hicolor/scalable/apps/thegn.svg"
[[ -f $desktop ]] || {
  echo "install did not write the .desktop entry ($desktop)" >&2
  exit 1
}
[[ $(<"$desktop") == *"Exec=$tmp/bin/tg --standalone"* ]] || {
  echo ".desktop entry should Exec 'tg --standalone'" >&2
  cat "$desktop" >&2
  exit 1
}
[[ $(<"$desktop") == *$'\nIcon=thegn\n'* ]] || {
  echo ".desktop entry should reference Icon=thegn" >&2
  cat "$desktop" >&2
  exit 1
}
[[ -f $icon && $(head -1 "$icon") == '<?xml'* ]] || {
  echo "install did not write the owl SVG icon ($icon)" >&2
  exit 1
}

echo "install plan checks passed"
