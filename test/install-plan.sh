#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

out="$("$repo"/install.sh --dry-run "$tmp/bin")"
[[ $out == *"$tmp/bin/szhost -> $repo/target/release/szhost"* ]] || {
  echo "dry-run did not plan szhost alias" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"$tmp/bin/superzej -> $repo/target/release/szhost"* ]] || {
  echo "dry-run did not plan superzej -> szhost" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"$tmp/bin/sj-tui wrapper -> $repo/target/release/szhost (current terminal)"* ]] || {
  echo "dry-run did not plan sj-tui current-terminal wrapper" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"$tmp/bin/sj wrapper -> alacritty --config-file $repo/config/alacritty.toml -e $tmp/bin/sj-tui"* ]] || {
  echo "dry-run did not plan sj dedicated alacritty wrapper" >&2
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
printf '%s\n' "$@" >"${SJ_ALACRITTY_LOG:?}"
EOF
chmod 0755 "$fakebin/alacritty"

install_out="$(PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" "$repo/install.sh" "$tmp/bin")"
[[ -L $tmp/bin/superzej && $(readlink "$tmp/bin/superzej") == "$repo/target/release/szhost" ]] || {
  echo "install did not symlink superzej to szhost" >&2
  echo "$install_out" >&2
  exit 1
}
[[ -L $tmp/bin/szhost && $(readlink "$tmp/bin/szhost") == "$repo/target/release/szhost" ]] || {
  echo "install did not symlink szhost to release binary" >&2
  echo "$install_out" >&2
  exit 1
}
[[ -x $tmp/bin/sj && -x $tmp/bin/sj-tui ]] || {
  echo "install did not create executable sj and sj-tui wrappers" >&2
  echo "$install_out" >&2
  exit 1
}
[[ $(<"$tmp/bin/sj-tui") == *"exec $repo/target/release/szhost"* ]] || {
  echo "sj-tui should exec szhost directly in the current terminal" >&2
  sed -n '1,120p' "$tmp/bin/sj-tui" >&2
  exit 1
}
[[ $(<"$tmp/bin/sj") == *"exec alacritty --config-file $repo/config/alacritty.toml -e $tmp/bin/sj-tui"* ]] || {
  echo "sj should launch the dedicated alacritty profile" >&2
  sed -n '1,120p' "$tmp/bin/sj" >&2
  exit 1
}
SJ_ALACRITTY_LOG="$tmp/alacritty.args" PATH="$fakebin:$PATH" "$tmp/bin/sj"
alacritty_args="$(<"$tmp/alacritty.args")"
[[ $alacritty_args == *$'--config-file\n'"$repo/config/alacritty.toml"*$'\n-e\n'"$tmp/bin/sj-tui"* ]] || {
  echo "sj did not invoke alacritty with the bundled config and sj-tui" >&2
  printf '%s\n' "$alacritty_args" >&2
  exit 1
}

echo "install plan checks passed"
