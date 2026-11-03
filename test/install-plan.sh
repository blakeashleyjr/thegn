#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# install.sh locates the staged release binary relative to its own path. Run it
# from a throwaway repo layout so every build artifact it reads or writes belongs
# to this test, never to the checkout running the test.
install_repo="$tmp/repo"
mkdir -p "$install_repo/target/release"
cp "$repo/install.sh" "$install_repo/install.sh"
cp -R "$repo/config" "$install_repo/config"
mkdir -p "$install_repo/packaging"
cp -R "$repo/packaging/macos/." "$install_repo/packaging/macos/"
printf '#!/usr/bin/env sh\nexit 0\n' >"$install_repo/target/release/thegn"
chmod 0755 "$install_repo/target/release/thegn"

# Launcher integration is per-platform, because the registry is: a freedesktop
# `.desktop` entry on Linux/BSD, a generated `thegn.app` bundle on macOS, and
# nothing on Windows. Mirror install.sh's predicates EXACTLY (keep these in
# sync), and assert each branch actually produces its own artifacts rather than
# dropping the coverage on the platforms that skip the other's.
freedesktop=1
macos=0
case "$(uname -s)" in
Darwin)
  freedesktop=0
  macos=1
  ;;
MINGW* | MSYS* | CYGWIN*) freedesktop=0 ;;
esac

out="$("$install_repo"/install.sh --dry-run "$tmp/bin")"
[[ $out == *"$tmp/bin/thegn <- copy of $install_repo/target/release/thegn"* ]] || {
  echo "dry-run did not plan the thegn binary copy" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"$tmp/bin/tg-tui wrapper -> $tmp/bin/thegn (current terminal)"* ]] || {
  echo "dry-run did not plan tg-tui current-terminal wrapper" >&2
  echo "$out" >&2
  exit 1
}
[[ $out == *"$tmp/bin/tg wrapper -> $tmp/bin/tg-tui (current terminal); tg -s|--standalone -> alacritty --config-file $install_repo/config/alacritty.toml -e $tmp/bin/tg-tui"* ]] || {
  echo "dry-run did not plan tg current-terminal + alacritty standalone wrapper" >&2
  echo "$out" >&2
  exit 1
}
if ((freedesktop)); then
  [[ $out == *"app-launcher entry (Exec=$tmp/bin/tg --standalone, Icon=thegn)"* ]] || {
    echo "dry-run did not plan the desktop entry with Exec=tg --standalone / Icon=thegn" >&2
    echo "$out" >&2
    exit 1
  }
elif ((macos)); then
  [[ $out == *"/Applications/thegn.app -> Spotlight/Raycast/Dock launcher"* ]] || {
    echo "dry-run should plan the macOS app bundle" >&2
    echo "$out" >&2
    exit 1
  }
  [[ $out != *".desktop"* ]] || {
    echo "dry-run must not plan a .desktop entry on macOS" >&2
    echo "$out" >&2
    exit 1
  }
else
  [[ $out == *"no launcher entry"* ]] || {
    echo "dry-run should report that this platform gets no launcher entry" >&2
    echo "$out" >&2
    exit 1
  }
fi
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
cat >"$fakebin/desktop-file-validate" <<'EOF'
#!/usr/bin/env sh
printf '%s\n' "$1" >>"${TG_DFV_LOG:-/dev/null}"
printf '%s\n' "${TG_DFV_MESSAGE-}"
exit "${TG_DFV_STATUS:-0}"
EOF
chmod 0755 "$fakebin/desktop-file-validate"

# install.sh copies the built binary, so the stub above lives at the expected
# path in the throwaway repo. The fake cargo builds nothing.
cleanup() {
  rm -rf "$tmp"
  return 0
}
trap cleanup EXIT

install_out="$(PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" "$install_repo/install.sh" "$tmp/bin")"
plain_tg_before="$(<"$tmp/bin/tg")"
plain_tui_before="$(<"$tmp/bin/tg-tui")"
# A COPY, not a symlink: the install must survive `cargo clean` / a moved repo.
[[ -f $tmp/bin/thegn && ! -L $tmp/bin/thegn && -x $tmp/bin/thegn ]] || {
  echo "install did not copy the release binary to bindir/thegn" >&2
  echo "$install_out" >&2
  exit 1
}
[[ -x $tmp/bin/tg && -x $tmp/bin/tg-tui ]] || {
  echo "install did not create executable tg and tg-tui wrappers" >&2
  echo "$install_out" >&2
  exit 1
}
[[ -f $tmp/config/thegn/config.toml && ! -L $tmp/config/thegn/config.toml ]] || {
  echo "install did not create a regular user config" >&2
  exit 1
}
config_before="$(<"$tmp/config/thegn/config.toml")"
[[ $(<"$tmp/bin/tg-tui") == *"exec $tmp/bin/thegn"* ]] || {
  echo "tg-tui should exec the INSTALLED thegn, not the source tree" >&2
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
[[ $(<"$tmp/bin/tg") == *"exec alacritty --config-file $install_repo/config/alacritty.toml -e $tmp/bin/tg-tui"* ]] || {
  echo "tg --standalone should launch the dedicated alacritty profile" >&2
  sed -n '1,120p' "$tmp/bin/tg" >&2
  exit 1
}
TG_ALACRITTY_LOG="$tmp/alacritty.args" PATH="$fakebin:$PATH" "$tmp/bin/tg" --standalone
alacritty_args="$(<"$tmp/alacritty.args")"
[[ $alacritty_args == *$'--config-file\n'"$install_repo/config/alacritty.toml"$'\n-e\n'"$tmp/bin/tg-tui"* ]] || {
  echo "tg --standalone did not invoke alacritty with the bundled config and tg-tui" >&2
  printf '%s\n' "$alacritty_args" >&2
  exit 1
}

# The desktop entry + owl icon must be installed under XDG_DATA_HOME (Linux
# only — on macOS neither is written, and neither may be).
desktop="$tmp/data/applications/thegn.desktop"
icon="$tmp/data/icons/hicolor/scalable/apps/thegn.svg"
if ((freedesktop)); then
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
  [[ $(<"$icon") == *'<!-- generated by thegn install.sh -->'* ]] || {
    echo "installed icon is missing its ownership marker" >&2
    exit 1
  }
else
  [[ ! -e $desktop && ! -e $icon ]] || {
    echo "install wrote freedesktop assets on a platform without a freedesktop launcher ($desktop / $icon)" >&2
    exit 1
  }
fi

# macOS gets an app bundle instead — the launcher registry Spotlight/Raycast/the
# Dock actually index. HOME is the sandbox above, so this asserts against the
# test's own ~/Applications and never touches the real one.
if ((macos)); then
  app="$tmp/home/Applications/thegn.app"
  [[ -d $app ]] || {
    echo "install did not generate the macOS app bundle ($app)" >&2
    echo "$install_out" >&2
    exit 1
  }
  [[ -x $app/Contents/MacOS/thegn ]] || {
    echo "app bundle has no executable launcher ($app/Contents/MacOS/thegn)" >&2
    exit 1
  }
  [[ -f $app/Contents/Resources/thegn.icns ]] || {
    echo "app bundle is missing the owl icon" >&2
    exit 1
  }
  [[ -f $app/Contents/Resources/thegn.command ]] || {
    echo "app bundle is missing the Terminal.app fallback runner" >&2
    exit 1
  }
  # The launcher must point at the INSTALLED binary, not the source tree — same
  # contract as the tg/tg-tui wrappers above (survives `cargo clean`).
  [[ $(<"$app/Contents/Resources/common.sh") == *"$tmp/bin/thegn"* ]] || {
    echo "app bundle should launch the installed thegn, not the source tree" >&2
    sed -n '1,40p' "$app/Contents/Resources/common.sh" >&2
    exit 1
  }
  if command -v plutil >/dev/null 2>&1; then
    plutil -lint "$app/Contents/Info.plist" >/dev/null || {
      echo "app bundle Info.plist is not a valid plist" >&2
      exit 1
    }
  fi
fi

# A foreign short command is a collision, including in dry-run. Preflight runs
# before the fake build and must leave both it and the binary destination alone.
foreign_bin="$tmp/foreign-bin"
mkdir -p "$foreign_bin"
printf '#!/bin/sh\necho other\n' >"$foreign_bin/tg"
foreign_tg_before="$(<"$foreign_bin/tg")"
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" "$foreign_bin" >"$tmp/foreign.out" 2>&1; then
  echo "install accepted a foreign tg collision" >&2
  exit 1
fi
[[ $(<"$foreign_bin/tg") == "$foreign_tg_before" && ! -e $foreign_bin/thegn ]] || {
  echo "foreign tg collision changed the target or wrote the binary" >&2
  exit 1
}
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" --dry-run "$foreign_bin" >"$tmp/foreign-dry.out" 2>&1; then
  echo "dry-run accepted a foreign tg collision" >&2
  exit 1
fi
[[ $(<"$tmp/foreign-dry.out") == *"refusing $foreign_bin/tg"* ]] || {
  echo "dry-run did not report the foreign tg collision" >&2
  cat "$tmp/foreign-dry.out" >&2
  exit 1
}

# Dangling destination symlinks are detected with -L and never followed.
symlink_bin="$tmp/symlink-bin"
mkdir -p "$symlink_bin"
ln -s "$tmp/outside" "$symlink_bin/tg-tui"
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" "$symlink_bin" >"$tmp/symlink.out" 2>&1; then
  echo "install accepted a tg-tui symlink collision" >&2
  exit 1
fi
[[ ! -e $tmp/outside && -L $symlink_bin/tg-tui ]] || {
  echo "tg-tui symlink collision followed or replaced the symlink" >&2
  exit 1
}

foreign_binary_bin="$tmp/foreign-binary-bin"
mkdir -p "$foreign_binary_bin"
printf 'foreign binary bytes\n' >"$foreign_binary_bin/thegn"
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" "$foreign_binary_bin" >"$tmp/foreign-binary.out" 2>&1; then
  echo "install accepted a foreign thegn binary" >&2
  exit 1
fi
[[ $(<"$foreign_binary_bin/thegn") == 'foreign binary bytes' && ! -e $foreign_binary_bin/tg ]] || {
  echo "foreign binary collision was changed or installation began" >&2
  exit 1
}

if ((freedesktop)); then
  for unsafe_suffix in 'with space' 'with%field'; do
    unsafe_bin="$tmp/$unsafe_suffix"
    if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
      "$install_repo/install.sh" --dry-run "$unsafe_bin" >"$tmp/unsafe-path.out" 2>&1; then
      echo "dry-run accepted an unsafe freedesktop bindir: $unsafe_bin" >&2
      exit 1
    fi
    [[ ! -e $unsafe_bin ]] || {
      echo "unsafe bindir validation created a path: $unsafe_bin" >&2
      exit 1
    }
  done

  # A foreign launcher or icon is also a collision, even with a clean bindir.
  collision_bin="$tmp/desktop-collision-bin"
  collision_data="$tmp/desktop-collision-data"
  mkdir -p "$collision_bin" "$collision_data/applications"
  printf 'foreign desktop\n' >"$collision_data/applications/thegn.desktop"
  if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$collision_data" \
    "$install_repo/install.sh" "$collision_bin" >"$tmp/desktop-collision.out" 2>&1; then
    echo "install accepted a foreign desktop entry" >&2
    exit 1
  fi
  [[ $(<"$collision_data/applications/thegn.desktop") == 'foreign desktop' && ! -e $collision_bin/thegn ]] || {
    echo "foreign desktop collision was changed or installation began" >&2
    exit 1
  }

  collision_bin="$tmp/icon-collision-bin"
  collision_data="$tmp/icon-collision-data"
  mkdir -p "$collision_bin" "$collision_data/icons/hicolor/scalable/apps"
  printf 'foreign icon\n' >"$collision_data/icons/hicolor/scalable/apps/thegn.svg"
  if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$collision_data" \
    "$install_repo/install.sh" "$collision_bin" >"$tmp/icon-collision.out" 2>&1; then
    echo "install accepted a foreign icon" >&2
    exit 1
  fi
  [[ $(<"$collision_data/icons/hicolor/scalable/apps/thegn.svg") == 'foreign icon' && ! -e $collision_bin/thegn ]] || {
    echo "foreign icon collision was changed or installation began" >&2
    exit 1
  }
fi

# Existing user configuration is preserved, and a dangling config symlink is
# a collision instead of a destination to follow.
[[ $(<"$tmp/config/thegn/config.toml") == "$config_before" ]] || {
  echo "reinstall changed existing user configuration" >&2
  exit 1
}
config_link_home="$tmp/config-link-home"
mkdir -p "$config_link_home/thegn" "$tmp/config-link-bin"
ln -s "$tmp/config-link-target" "$config_link_home/thegn/config.toml"
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$config_link_home" XDG_DATA_HOME="$tmp/config-link-data" \
  "$install_repo/install.sh" "$tmp/config-link-bin" >"$tmp/config-link.out" 2>&1; then
  echo "install accepted a config symlink collision" >&2
  exit 1
fi
[[ ! -e $tmp/config-link-target && -L $config_link_home/thegn/config.toml && ! -e $tmp/config-link-bin/thegn ]] || {
  echo "config symlink collision was followed or installation began" >&2
  exit 1
}

# The marker permits an upgrade over artifacts produced by this installer.
PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" "$tmp/bin" >"$tmp/reinstall.out"
[[ $(<"$tmp/bin/tg") == *'# generated by thegn install.sh'* && $(<"$tmp/bin/tg-tui") == *'# generated by thegn install.sh'* ]] || {
  echo "reinstall did not retain installer ownership markers" >&2
  exit 1
}

# Existing installation identity comes from its wrapper, not a possibly stale
# staged release binary (which may differ after a build or target reclaim).
printf 'changed staged release bytes\n' >"$install_repo/target/release/thegn"
if ! PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" "$tmp/bin" >"$tmp/changed-release-reinstall.out" 2>&1; then
  echo "reinstall refused changed staged release bytes" >&2
  cat "$tmp/changed-release-reinstall.out" >&2
  exit 1
fi
[[ $(<"$tmp/bin/thegn") == 'changed staged release bytes' ]] || {
  echo "reinstall did not update the installed binary from changed staged bytes" >&2
  exit 1
}

# A live per-bindir lock refuses before build or destination mutation.
lock_snapshot="$(cksum "$tmp/bin/thegn" "$tmp/bin/tg-tui" "$tmp/bin/tg")"
mkdir "$tmp/bin/.thegn-install.lock"
printf '%s\n' "$$" >"$tmp/bin/.thegn-install.lock/pid"
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" "$tmp/bin" >"$tmp/live-lock.out" 2>&1; then
  echo "install did not refuse a live lock" >&2
  exit 1
fi
[[ $(<"$tmp/live-lock.out") == *"another install is running"* ]] || {
  echo "live lock refusal did not identify another installer" >&2
  cat "$tmp/live-lock.out" >&2
  exit 1
}
[[ $(cksum "$tmp/bin/thegn" "$tmp/bin/tg-tui" "$tmp/bin/tg") == "$lock_snapshot" ]] || {
  echo "live lock refusal changed installed files" >&2
  exit 1
}
rm -rf -- "$tmp/bin/.thegn-install.lock"

# A freshly created lock without a pid is treated as live while its owner is
# publishing the pid file.
mkdir "$tmp/bin/.thegn-install.lock"
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" "$tmp/bin" >"$tmp/pidless-lock.out" 2>&1; then
  echo "install reclaimed a lock before its owner published the pid" >&2
  exit 1
fi
[[ $(<"$tmp/pidless-lock.out") == *"another install is starting"* && -d $tmp/bin/.thegn-install.lock ]] || {
  echo "pidless lock was not treated as live during publication" >&2
  cat "$tmp/pidless-lock.out" >&2
  exit 1
}
rm -rf -- "$tmp/bin/.thegn-install.lock"

# A dead owner's lock is reclaimed and removed after the successful install.
sh -c 'exit 0' &
dead_pid=$!
wait "$dead_pid"
mkdir "$tmp/bin/.thegn-install.lock"
printf '%s\n' "$dead_pid" >"$tmp/bin/.thegn-install.lock/pid"
if ! PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" "$tmp/bin" >"$tmp/stale-lock.out" 2>&1; then
  echo "install did not reclaim a stale lock" >&2
  cat "$tmp/stale-lock.out" >&2
  exit 1
fi
[[ ! -e $tmp/bin/.thegn-install.lock ]] || {
  echo "successful install left its lock directory" >&2
  exit 1
}

# An injected failure after tg restores all three replaced command artifacts.
cp "$tmp/bin/thegn" "$tmp/thegn.before"
cp "$tmp/bin/tg-tui" "$tmp/tg-tui.before"
cp "$tmp/bin/tg" "$tmp/tg.before"
manifest_before="$(cksum "$tmp/data/thegn/install-manifest")"
printf 'new staged bytes for rollback\n' >"$install_repo/target/release/thegn"
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  THEGN_INSTALL_FAIL_AFTER=tg "$install_repo/install.sh" "$tmp/bin" >"$tmp/rollback.out" 2>&1; then
  echo "injected tg failure unexpectedly succeeded" >&2
  exit 1
fi
if ! cmp -s "$tmp/thegn.before" "$tmp/bin/thegn" || ! cmp -s "$tmp/tg-tui.before" "$tmp/bin/tg-tui" ||
  ! cmp -s "$tmp/tg.before" "$tmp/bin/tg"; then
  echo "injected tg failure did not restore the previous command files" >&2
  cat "$tmp/rollback.out" >&2
  exit 1
fi
[[ $(<"$tmp/rollback.out") == *"restored the previous installation"* ]] || {
  echo "rollback did not report restoration" >&2
  cat "$tmp/rollback.out" >&2
  exit 1
}
[[ $(cksum "$tmp/data/thegn/install-manifest") == "$manifest_before" ]] || {
  echo "rollback changed the manifest before it was committed" >&2
  exit 1
}
shopt -s nullglob
install_leftovers=("$tmp/bin"/.thegn-install-backup.* "$tmp/bin"/.thegn-install.*)
shopt -u nullglob
if ((${#install_leftovers[@]} > 0)); then
  echo "rollback left a backup, staging file, or lock in bindir" >&2
  printf '%s\n' "${install_leftovers[@]}" >&2
  exit 1
fi

# A clean install failing after its first commit removes that new artifact.
clean_fail_bin="$tmp/clean-fail-bin"
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/clean-fail-config" XDG_DATA_HOME="$tmp/clean-fail-data" \
  THEGN_INSTALL_FAIL_AFTER=thegn "$install_repo/install.sh" "$clean_fail_bin" >"$tmp/clean-fail.out" 2>&1; then
  echo "injected first-artifact failure unexpectedly succeeded" >&2
  exit 1
fi
[[ ! -e $clean_fail_bin/thegn && ! -e $clean_fail_bin/.thegn-install.lock ]] || {
  echo "failed clean install left a binary or lock" >&2
  exit 1
}

# The versioned manifest is published last and hashes the installed artifacts.
manifest="$tmp/data/thegn/install-manifest"
[[ $(head -n 1 "$manifest") == 'thegn-install-manifest 1' ]] || {
  echo "manifest is missing its version header" >&2
  exit 1
}
test_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}
for artifact in "$tmp/bin/thegn" "$tmp/bin/tg-tui" "$tmp/bin/tg"; do
  artifact_hash="$(test_sha256 "$artifact")"
  grep -Fqx -- "$artifact_hash$(printf '\t')$artifact" "$manifest" || {
    echo "manifest does not contain the installed hash and path for $artifact" >&2
    exit 1
  }
done
if ((freedesktop)); then
  for artifact in "$icon" "$desktop"; do
    artifact_hash="$(test_sha256 "$artifact")"
    grep -Fqx -- "$artifact_hash$(printf '\t')$artifact" "$manifest" || {
      echo "manifest does not contain the installed hash and path for $artifact" >&2
      exit 1
    }
  done

  manifest_fail_bin="$tmp/manifest-fail-bin"
  manifest_fail_data="$tmp/manifest-fail-data"
  if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/manifest-fail-config" XDG_DATA_HOME="$manifest_fail_data" \
    THEGN_INSTALL_FAIL_AFTER=desktop "$install_repo/install.sh" "$manifest_fail_bin" >"$tmp/manifest-fail.out" 2>&1; then
    echo "injected desktop failure unexpectedly succeeded" >&2
    exit 1
  fi
  [[ ! -e $manifest_fail_data/thegn/install-manifest ]] || {
    echo "installer wrote a manifest before the desktop artifact completed" >&2
    exit 1
  }
fi

# Destination bindir symlinks are refused by both dry-run and real preflight.
mkdir "$tmp/real-bin"
ln -s "$tmp/real-bin" "$tmp/link-bin"
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/link-config" XDG_DATA_HOME="$tmp/link-data" \
  "$install_repo/install.sh" --dry-run "$tmp/link-bin" >"$tmp/link-dry.out" 2>&1; then
  echo "dry-run accepted a symlinked bindir" >&2
  exit 1
fi
[[ $(<"$tmp/link-dry.out") == *"is a symlink"* ]] || {
  echo "dry-run symlink refusal did not name the conflict" >&2
  cat "$tmp/link-dry.out" >&2
  exit 1
}
if PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/link-config" XDG_DATA_HOME="$tmp/link-data" \
  "$install_repo/install.sh" "$tmp/link-bin" >"$tmp/link-real.out" 2>&1; then
  echo "real install accepted a symlinked bindir" >&2
  exit 1
fi
[[ $(<"$tmp/link-real.out") == *"is a symlink"* && ! -e $tmp/real-bin/thegn && ! -e $tmp/real-bin/tg &&
! -e $tmp/real-bin/tg-tui ]] || {
  echo "real symlink refusal wrote through the bindir" >&2
  cat "$tmp/link-real.out" >&2
  exit 1
}

# Exercise make-app input checks on Linux with a temporary Darwin platform shim.
mac_test_bin="$tmp/mac-bin"
mkdir -p "$mac_test_bin"
cat >"$mac_test_bin/uname" <<'EOF'
#!/usr/bin/env sh
printf 'Darwin\n'
EOF
chmod 0755 "$mac_test_bin/uname"
cat >"$mac_test_bin/thegn" <<'EOF'
#!/usr/bin/env sh
printf 'thegn 1.2.3\n'
EOF
chmod 0755 "$mac_test_bin/thegn"
make_app="$install_repo/packaging/macos/make-app.sh"
name_status=0
PATH="$mac_test_bin:$PATH" HOME="$tmp/home" "$make_app" --bin "$mac_test_bin/thegn" --dest "$tmp/mac-apps" --name ../escaped --dry-run >"$tmp/name.out" 2>&1 || name_status=$?
if ((name_status != 2)); then
  echo "make-app accepted a traversing name" >&2
  exit 1
fi
[[ $(<"$tmp/name.out") == *"--name must be"* ]] || {
  echo "make-app did not report the invalid --name" >&2
  cat "$tmp/name.out" >&2
  exit 1
}
[[ ! -e $tmp/escaped.app && ! -e $tmp/mac-apps ]] || {
  echo "invalid make-app name created a path" >&2
  exit 1
}
env_status=0
PATH="$mac_test_bin:$PATH" HOME="$tmp/home" "$make_app" --bin "$mac_test_bin/thegn" --dest "$tmp/mac-apps" --env 'SAFE;touch PWNED=x' --dry-run >"$tmp/env.out" 2>&1 || env_status=$?
if ((env_status != 2)); then
  echo "make-app accepted a shell syntax environment key" >&2
  exit 1
fi
[[ $(<"$tmp/env.out") == *"invalid --env key"* ]] || {
  echo "make-app did not report the invalid --env key" >&2
  cat "$tmp/env.out" >&2
  exit 1
}
cat >"$mac_test_bin/thegn" <<'EOF'
#!/usr/bin/env sh
printf 'thegn 1.2.3</string><key>X</key>\n'
EOF
chmod 0755 "$mac_test_bin/thegn"
version_status=0
PATH="$mac_test_bin:$PATH" HOME="$tmp/home" "$make_app" --bin "$mac_test_bin/thegn" --dest "$tmp/mac-apps" --dry-run >"$tmp/version.out" 2>&1 || version_status=$?
if ((version_status != 1)); then
  echo "make-app accepted injected version output" >&2
  exit 1
fi
[[ $(<"$tmp/version.out") == *"unsupported version output"* ]] || {
  echo "make-app did not report unsupported version output" >&2
  cat "$tmp/version.out" >&2
  exit 1
}
cat >"$mac_test_bin/thegn" <<'EOF'
#!/usr/bin/env sh
printf 'thegn 1.2.3\n'
EOF
chmod 0755 "$mac_test_bin/thegn"
PATH="$mac_test_bin:$PATH" HOME="$tmp/home" "$make_app" --bin "$mac_test_bin/thegn" --dest "$tmp/mac-apps" --env THEGN_LOG=info --dry-run >"$tmp/mac-dry.out"
[[ $(<"$tmp/mac-dry.out") == *"$tmp/mac-apps/thegn.app"* && ! -e $tmp/mac-apps ]] || {
  echo "valid make-app dry-run did not succeed without creating the bundle" >&2
  exit 1
}

if [[ $(uname -s) == Linux ]]; then
  # batteries: dry-run plan
  batteries_dry_out="$(PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/batteries-dry-config" XDG_DATA_HOME="$tmp/batteries-dry-data" \
    "$install_repo/install.sh" --dry-run --batteries "$tmp/batteries-dry-bin")"
  [[ $batteries_dry_out == *"$tmp/batteries-dry-data/fonts/thegn-nerd-fonts"* &&
    $batteries_dry_out == *"$tmp/batteries-dry-config/thegn/alacritty.toml"* &&
    $batteries_dry_out == *"(host): $fakebin/alacritty"* &&
    $batteries_dry_out == *"https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.0/FiraCode.tar.xz"* &&
    $batteries_dry_out == *"32226dc81bb30ea421cdc49ba2134b93c2b43096992c829504a3004d2537420d"* ]] || {
    echo "batteries dry-run omitted the terminal, font pin/destination, or config plan" >&2
    echo "$batteries_dry_out" >&2
    exit 1
  }
  [[ ! -e $tmp/batteries-dry-data ]] || {
    echo "batteries dry-run created data files" >&2
    exit 1
  }

  # Create a tiny test archive and pin only the throwaway installer copy to its
  # digest. The production pin remains the Nerd Fonts release digest.
  mkdir -p "$tmp/fake-fonts"
  printf 'test font bytes\n' >"$tmp/fake-fonts/FiraCodeNerdFont-Regular.ttf"
  tar -cJf "$tmp/FiraCode.tar.xz" -C "$tmp/fake-fonts" FiraCodeNerdFont-Regular.ttf
  if command -v sha256sum >/dev/null 2>&1; then
    fixture_font_sha="$(sha256sum "$tmp/FiraCode.tar.xz" | cut -d' ' -f1)"
  else
    fixture_font_sha="$(shasum -a 256 "$tmp/FiraCode.tar.xz" | cut -d' ' -f1)"
  fi
  sed -i.bak "s/^batteries_font_sha256=.*/batteries_font_sha256=\"$fixture_font_sha\"/" "$install_repo/install.sh"
  rm -f "$install_repo/install.sh.bak"

  # batteries: install with host alacritty
  THEGN_INSTALL_BATTERIES_FONT_SRC="$tmp/FiraCode.tar.xz" TG_ALACRITTY_LOG="$tmp/batteries-alacritty.log" \
    PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
    "$install_repo/install.sh" --batteries "$tmp/bin" >"$tmp/batteries-install.out"
  [[ -f $tmp/data/fonts/thegn-nerd-fonts/FiraCodeNerdFont-Regular.ttf &&
    $(<"$tmp/data/thegn/batteries/marker") == *"alacritty_source=host"* &&
    -f $tmp/config/thegn/alacritty.toml ]] || {
    echo "batteries install did not publish the verified font, host marker, and config" >&2
    cat "$tmp/batteries-install.out" >&2
    exit 1
  }
  TG_ALACRITTY_LOG="$tmp/batteries-alacritty.log" PATH="$fakebin:$PATH" "$tmp/bin/tg" --standalone
  [[ $(<"$tmp/batteries-alacritty.log") == *"$tmp/config/thegn/alacritty.toml"* ]] || {
    echo "batteries standalone wrapper did not pass the user-local config" >&2
    cat "$tmp/batteries-alacritty.log" >&2
    exit 1
  }

  # batteries: digest mismatch refuses and rolls back
  PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/mismatch-config" XDG_DATA_HOME="$tmp/mismatch-data" \
    "$install_repo/install.sh" "$tmp/mismatch-bin" >"$tmp/mismatch-plain.out"
  prior_tg="$(<"$tmp/mismatch-bin/tg")"
  printf 'different archive bytes\n' >"$tmp/bad-font"
  tar -cJf "$tmp/bad-FiraCode.tar.xz" -C "$tmp" bad-font
  bad_status=0
  THEGN_INSTALL_BATTERIES_FONT_SRC="$tmp/bad-FiraCode.tar.xz" PATH="$fakebin:$PATH" HOME="$tmp/home" \
    XDG_CONFIG_HOME="$tmp/mismatch-config" XDG_DATA_HOME="$tmp/mismatch-data" "$install_repo/install.sh" --batteries "$tmp/mismatch-bin" \
    >"$tmp/batteries-bad.out" 2>&1 || bad_status=$?
  [[ $bad_status == 1 && $(<"$tmp/batteries-bad.out") == *"digest mismatch"* &&
  $(<"$tmp/mismatch-bin/tg") == "$prior_tg" &&
  ! -e $tmp/mismatch-data/fonts/thegn-nerd-fonts/FiraCodeNerdFont-Regular.ttf &&
  ! -e $tmp/mismatch-data/thegn/batteries/marker ]] || {
    echo "batteries digest mismatch did not refuse and restore the existing install" >&2
    cat "$tmp/batteries-bad.out" >&2
    exit 1
  }

  # batteries: build prerequisites refused
  mkdir -p "$tmp/no-pkg-bin"
  cat >"$tmp/no-pkg-bin/pkg-config" <<'EOF'
#!/usr/bin/env sh
exit 1
EOF
  chmod 0755 "$tmp/no-pkg-bin/pkg-config"
  prereq_status=0
  THEGN_INSTALL_ASSUME_NO_HOST_ALACRITTY=1 PATH="$tmp/no-pkg-bin:$PATH" HOME="$tmp/home" \
    XDG_CONFIG_HOME="$tmp/prereq-config" XDG_DATA_HOME="$tmp/prereq-data" \
    "$install_repo/install.sh" --batteries "$tmp/prereq-bin" >"$tmp/prereq.out" 2>&1 || prereq_status=$?
  [[ $prereq_status == 1 && $(<"$tmp/prereq.out") == *"pkg-config modules fontconfig freetype2 xkbcommon"* &&
  ! -e $tmp/prereq-bin && ! -e $tmp/prereq-data ]] || {
    echo "batteries prerequisite refusal did not name missing pkg-config modules before installing" >&2
    cat "$tmp/prereq.out" >&2
    exit 1
  }

  # batteries: user-local alacritty build
  mkdir -p "$tmp/build-bin"
  cp "$fakebin/desktop-file-validate" "$tmp/build-bin/desktop-file-validate"
  cat >"$tmp/build-bin/cmake" <<'EOF'
#!/usr/bin/env sh
exit 0
EOF
  cat >"$tmp/build-bin/pkg-config" <<'EOF'
#!/usr/bin/env sh
exit 0
EOF
  cat >"$tmp/build-bin/cargo" <<'EOF'
#!/usr/bin/env sh
printf '%s\n' "$*" >>"${TG_CARGO_LOG:?}"
if [ "$1" = install ]; then
  shift
  root=
  while [ "$#" -gt 0 ]; do
    if [ "$1" = --root ]; then shift; root=$1; fi
    shift
  done
  mkdir -p "$root/bin"
  printf '#!/usr/bin/env sh\nexit 0\n' >"$root/bin/alacritty"
  chmod 0755 "$root/bin/alacritty"
fi
EOF
  chmod 0755 "$tmp/build-bin/cmake" "$tmp/build-bin/pkg-config" "$tmp/build-bin/cargo"
  TG_CARGO_LOG="$tmp/cargo-install.log" THEGN_INSTALL_ASSUME_NO_HOST_ALACRITTY=1 \
    THEGN_INSTALL_BATTERIES_FONT_SRC="$tmp/FiraCode.tar.xz" PATH="$tmp/build-bin:$PATH" HOME="$tmp/build-home" \
    XDG_CONFIG_HOME="$tmp/build-config" XDG_DATA_HOME="$tmp/build-data" \
    "$install_repo/install.sh" --batteries "$tmp/build-bindir" >"$tmp/build-install.out"
  [[ $(<"$tmp/cargo-install.log") == *"install --locked --root $tmp/build-data/thegn/batteries alacritty --version 0.17.0"* &&
  $(<"$tmp/build-data/thegn/batteries/marker") == *"alacritty_source=user-local"* ]] || {
    echo "batteries did not build and record the pinned user-local Alacritty" >&2
    cat "$tmp/cargo-install.log" "$tmp/build-install.out" >&2
    exit 1
  }

  # batteries: user-local launcher paths containing spaces remain literal
  space_data="$tmp/data root with spaces"
  space_config="$tmp/config root with spaces"
  THEGN_INSTALL_ASSUME_NO_HOST_ALACRITTY=1 TG_CARGO_LOG="$tmp/space-cargo.log" \
    THEGN_INSTALL_BATTERIES_FONT_SRC="$tmp/FiraCode.tar.xz" PATH="$tmp/build-bin:$PATH" HOME="$tmp/build-home" \
    XDG_CONFIG_HOME="$space_config" XDG_DATA_HOME="$space_data" \
    "$install_repo/install.sh" --batteries "$tmp/space-bindir" >"$tmp/space-install.out"
  cat >"$space_data/thegn/batteries/bin/alacritty" <<'EOF'
#!/usr/bin/env sh
printf '%s\n' "$@" >"${TG_ALACRITTY_LOG:?}"
EOF
  chmod 0755 "$space_data/thegn/batteries/bin/alacritty"
  TG_ALACRITTY_LOG="$tmp/space-alacritty.log" "$tmp/space-bindir/tg" --standalone
  [[ $(<"$tmp/space-alacritty.log") == *"$space_config/thegn/alacritty.toml"* ]] || {
    echo "batteries standalone wrapper failed with spaces in XDG paths" >&2
    cat "$tmp/space-install.out" >&2
    exit 1
  }

  # batteries: existing alacritty.toml is kept
  printf 'user-owned alacritty config\n' >"$tmp/config/thegn/alacritty.toml"
  THEGN_INSTALL_BATTERIES_FONT_SRC="$tmp/FiraCode.tar.xz" PATH="$fakebin:$PATH" HOME="$tmp/home" \
    XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" "$install_repo/install.sh" --batteries "$tmp/bin" \
    >"$tmp/batteries-kept.out"
  [[ $(<"$tmp/config/thegn/alacritty.toml") == "user-owned alacritty config" ]] || {
    echo "batteries install replaced an existing user Alacritty config" >&2
    exit 1
  }

# batteries: non-Linux refusal path is present
fi
mkdir -p "$tmp/nonlinux-bin"
cat >"$tmp/nonlinux-bin/uname" <<'EOF'
#!/usr/bin/env sh
printf 'Darwin\n'
EOF
chmod 0755 "$tmp/nonlinux-bin/uname"
nonlinux_status=0
PATH="$tmp/nonlinux-bin:$PATH" HOME="$tmp/nonlinux-home" XDG_CONFIG_HOME="$tmp/nonlinux-config" \
  XDG_DATA_HOME="$tmp/nonlinux-data" "$install_repo/install.sh" --batteries --dry-run "$tmp/nonlinux-bindir" \
  >"$tmp/nonlinux.out" 2>&1 || nonlinux_status=$?
[[ $nonlinux_status == 2 && $(<"$tmp/nonlinux.out") == *"Linux-only"* && ! -e $tmp/nonlinux-bindir &&
! -e $tmp/nonlinux-data ]] || {
  echo "batteries mode did not refuse non-Linux before changing anything" >&2
  cat "$tmp/nonlinux.out" >&2
  exit 1
}

# batteries: plain install unchanged
PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" \
  "$install_repo/install.sh" "$tmp/bin" >"$tmp/plain-after-batteries.out"
[[ $(<"$tmp/bin/tg-tui") == *"export THEGN_ALACRITTY_CONFIG=$install_repo/config/alacritty.toml"* ]] || {
  echo "plain install wrapper no longer points at the checkout Alacritty profile" >&2
  exit 1
}
[[ $(<"$tmp/bin/tg") == "$plain_tg_before" && $(<"$tmp/bin/tg-tui") == "$plain_tui_before" ]] || {
  echo "plain install wrappers changed after enabling the batteries mode" >&2
  exit 1
}

install_uninstall_fixture() {
  local name="$1"
  case_home="$tmp/$name-home"
  case_config="$tmp/$name-config"
  case_data="$tmp/$name-data"
  case_bin="$tmp/$name-bin"
  case_manifest="$case_data/thegn/install-manifest"
  case_config_file="$case_config/thegn/config.toml"
  mkdir -p "$case_home"
  PATH="$fakebin:$PATH" HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
    "$install_repo/install.sh" "$case_bin" >"$tmp/$name-install.out"
  cp "$case_config_file" "$tmp/$name-config-original"
}

# uninstall-dry-run: every tracked artifact gets a plan row; all bytes and the lock state stay untouched.
install_uninstall_fixture uninstall-dry-run
uninstall_status=0
uninstall_out="$(HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall --dry-run "$case_bin" 2>&1)" || uninstall_status=$?
[[ $uninstall_status == 0 ]] || {
  echo "uninstall dry-run failed" >&2
  echo "$uninstall_out" >&2
  exit 1
}
while IFS=$'\t' read -r _ uninstall_path; do
  [[ -n $uninstall_path ]] || continue
  [[ $uninstall_out == *"would remove $uninstall_path"* ]] || {
    echo "uninstall dry-run omitted $uninstall_path" >&2
    echo "$uninstall_out" >&2
    exit 1
  }
done < <(tail -n +2 "$case_manifest")
[[ -e $case_bin/thegn && -e $case_bin/tg && -e $case_bin/tg-tui && -e $case_manifest && ! -e $case_bin/.thegn-install.lock ]] || {
  echo "uninstall dry-run changed installed artifacts, the manifest, or the lock" >&2
  exit 1
}
if ((freedesktop)); then
  [[ -e $case_data/icons/hicolor/scalable/apps/thegn.svg && -e $case_data/applications/thegn.desktop ]] || {
    echo "uninstall dry-run changed desktop integration artifacts" >&2
    exit 1
  }
fi
cmp -s -- "$tmp/uninstall-dry-run-config-original" "$case_config_file" || {
  echo "uninstall dry-run changed the user config" >&2
  exit 1
}

# uninstall-clean: tracked files and manifest disappear, while user config bytes remain intact.
install_uninstall_fixture uninstall-clean
uninstall_status=0
uninstall_out="$(HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall "$case_bin" 2>&1)" || uninstall_status=$?
[[ $uninstall_status == 0 ]] || {
  echo "clean uninstall failed" >&2
  echo "$uninstall_out" >&2
  exit 1
}
[[ ! -e $case_bin/thegn && ! -e $case_bin/tg && ! -e $case_bin/tg-tui && ! -e $case_manifest &&
  ! -e $case_bin/.thegn-install.lock ]] || {
  echo "clean uninstall left tracked artifacts, manifest, or lock" >&2
  echo "$uninstall_out" >&2
  exit 1
}
if ((freedesktop)); then
  [[ ! -e $case_data/icons/hicolor/scalable/apps/thegn.svg && ! -e $case_data/applications/thegn.desktop ]] || {
    echo "clean uninstall left desktop integration artifacts" >&2
    exit 1
  }
fi
cmp -s -- "$tmp/uninstall-clean-config-original" "$case_config_file" || {
  echo "clean uninstall changed the user config" >&2
  exit 1
}

# uninstall-keeps-modified: a changed wrapper stays, while unchanged artifacts are removed.
install_uninstall_fixture uninstall-keeps-modified
printf '\nuser change\n' >>"$case_bin/tg"
uninstall_status=0
uninstall_out="$(HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall "$case_bin" 2>&1)" || uninstall_status=$?
[[ $uninstall_status == 1 && $(<"$case_bin/tg") == *"user change"* && ! -e $case_bin/thegn &&
! -e $case_bin/tg-tui && -e $case_manifest &&
$uninstall_out == *"kept $case_bin/tg: changed since install"* ]] || {
  echo "modified artifact was not retained as an incomplete uninstall" >&2
  echo "$uninstall_out" >&2
  exit 1
}

# uninstall-refuses-malformed-manifest: an invalid record refuses before touching any installed path.
install_uninstall_fixture uninstall-refuses-malformed
printf 'not-a-hash\t%s\n' "$case_bin/thegn" >>"$case_manifest"
uninstall_status=0
uninstall_out="$(HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall "$case_bin" 2>&1)" || uninstall_status=$?
[[ $uninstall_status == 1 && -e $case_bin/thegn && -e $case_bin/tg && -e $case_bin/tg-tui &&
  -e $case_manifest && $uninstall_out == *"manifest line"* ]] || {
  echo "malformed manifest did not refuse before removal" >&2
  echo "$uninstall_out" >&2
  exit 1
}

# uninstall-refuses-duplicate-path: duplicate records refuse before any artifact is removed.
install_uninstall_fixture uninstall-refuses-duplicate
duplicate_manifest_line="$(sed -n '2p' "$case_manifest")"
printf '%s\n' "$duplicate_manifest_line" >>"$case_manifest"
uninstall_status=0
uninstall_out="$(HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall "$case_bin" 2>&1)" || uninstall_status=$?
[[ $uninstall_status == 1 && -e $case_bin/thegn && -e $case_bin/tg && -e $case_bin/tg-tui &&
  -e $case_manifest && $uninstall_out == *"duplicate path"* ]] || {
  echo "duplicate manifest path did not refuse before removal" >&2
  echo "$uninstall_out" >&2
  exit 1
}

# uninstall-refuses-oversized-manifest: the size cap is checked before any artifact removal.
install_uninstall_fixture uninstall-refuses-oversized
head -c 65537 /dev/zero >>"$case_manifest"
uninstall_status=0
uninstall_out="$(HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall "$case_bin" 2>&1)" || uninstall_status=$?
[[ $uninstall_status == 1 && -e $case_bin/thegn && -e $case_bin/tg && -e $case_bin/tg-tui &&
  -e $case_manifest && $uninstall_out == *"exceeds 65536 bytes"* ]] || {
  echo "oversized manifest did not refuse before removal" >&2
  echo "$uninstall_out" >&2
  exit 1
}

# uninstall-refuses-forged-entry: an out-of-root path rejects the whole manifest before removal.
install_uninstall_fixture uninstall-refuses-forged-entry
victim_file="$tmp/uninstall-forged-victim"
printf 'victim\n' >"$victim_file"
victim_hash="$(test_sha256 "$victim_file")"
printf '%s\t%s\n' "$victim_hash" "$victim_file" >>"$case_manifest"
uninstall_status=0
uninstall_out="$(HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall "$case_bin" 2>&1)" || uninstall_status=$?
[[ $uninstall_status == 1 && -e $case_bin/thegn && -e $case_bin/tg && -e $case_bin/tg-tui &&
  -e $victim_file && -e $case_manifest && $uninstall_out == *"outside this install's roots"* ]] || {
  echo "forged manifest entry did not refuse before removal" >&2
  echo "$uninstall_out" >&2
  exit 1
}

# uninstall-refuses-other-bindir: the manifest path set is bound to the supplied bindir.
install_uninstall_fixture uninstall-refuses-other-bindir
uninstall_status=0
uninstall_out="$(HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall "$tmp/uninstall-other-bin" 2>&1)" || uninstall_status=$?
[[ $uninstall_status == 1 && -e $case_bin/thegn && -e $case_bin/tg && -e $case_bin/tg-tui &&
  -e $case_manifest && $uninstall_out == *"outside this install's roots"* ]] || {
  echo "uninstall with another bindir did not refuse before removal" >&2
  echo "$uninstall_out" >&2
  exit 1
}

# uninstall-keeps-symlink: the link is kept and its target is never changed.
install_uninstall_fixture uninstall-keeps-symlink
victim_link_target="$tmp/uninstall-symlink-victim"
printf 'symlink victim\n' >"$victim_link_target"
rm -f -- "$case_bin/tg-tui"
ln -s "$victim_link_target" "$case_bin/tg-tui"
uninstall_status=0
uninstall_out="$(HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall "$case_bin" 2>&1)" || uninstall_status=$?
[[ $uninstall_status == 1 && -L $case_bin/tg-tui && $(<"$victim_link_target") == 'symlink victim' &&
! -e $case_bin/thegn && ! -e $case_bin/tg && -e $case_manifest && $uninstall_out == *"$case_bin/tg-tui"* ]] || {
  echo "uninstall did not keep a symlink and its target safely" >&2
  echo "$uninstall_out" >&2
  exit 1
}

# uninstall-then-reinstall: a clean uninstall permits a fresh install and manifest publication.
install_uninstall_fixture uninstall-then-reinstall
uninstall_status=0
HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall "$case_bin" >"$tmp/uninstall-then-reinstall-uninstall.out" 2>&1 || uninstall_status=$?
[[ $uninstall_status == 0 ]] || {
  echo "uninstall before reinstall failed" >&2
  cat "$tmp/uninstall-then-reinstall-uninstall.out" >&2
  exit 1
}
PATH="$fakebin:$PATH" HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" "$case_bin" >"$tmp/uninstall-then-reinstall.out"
[[ $(head -n 1 "$case_manifest") == 'thegn-install-manifest 1' && -x $case_bin/thegn && -x $case_bin/tg &&
-x $case_bin/tg-tui ]] || {
  echo "reinstall after uninstall did not publish a fresh complete install" >&2
  cat "$tmp/uninstall-then-reinstall.out" >&2
  exit 1
}

# uninstall-rejects-batteries: positive control; the existing unknown-option path already exits 2 at base.
install_uninstall_fixture uninstall-rejects-batteries
uninstall_status=0
HOME="$case_home" XDG_CONFIG_HOME="$case_config" XDG_DATA_HOME="$case_data" \
  "$install_repo/install.sh" --uninstall --batteries "$case_bin" >"$tmp/uninstall-rejects-batteries.out" 2>&1 ||
  uninstall_status=$?
[[ $uninstall_status == 2 && -e $case_bin/thegn && -e $case_manifest ]] || {
  echo "uninstall with --batteries did not refuse as a usage error" >&2
  cat "$tmp/uninstall-rejects-batteries.out" >&2
  exit 1
}

if ((freedesktop)); then
  # desktop-validate-staged: validation sees the private staged path before publication.
  dfv_bin="$tmp/dfv-bin"
  dfv_data="$tmp/dfv-data"
  dfv_log="$tmp/dfv.log"
  PATH="$fakebin:$PATH" HOME="$tmp/dfv-home" XDG_CONFIG_HOME="$tmp/dfv-config" \
    XDG_DATA_HOME="$dfv_data" TG_DFV_LOG="$dfv_log" \
    "$install_repo/install.sh" "$dfv_bin" >"$tmp/dfv-install.out" 2>&1
  [[ $(wc -l <"$dfv_log") -eq 1 ]] || {
    echo "desktop validator was not called exactly once for the staged entry" >&2
    cat "$tmp/dfv-install.out" >&2
    exit 1
  }
  dfv_logged_path="$(<"$dfv_log")"
  [[ $(dirname "$dfv_logged_path") == "$dfv_data/applications" &&
  $(basename "$dfv_logged_path") == .thegn-install.* && -f $dfv_data/applications/thegn.desktop ]] || {
    echo "desktop validator did not receive the staged entry before publication: $dfv_logged_path" >&2
    exit 1
  }

  # desktop-validate-refusal-rolls-back: validator failure preserves the prior install and manifest.
  cp "$dfv_bin/thegn" "$tmp/dfv-thegn.before"
  cp "$dfv_bin/tg" "$tmp/dfv-tg.before"
  cp "$dfv_bin/tg-tui" "$tmp/dfv-tg-tui.before"
  cp "$dfv_data/applications/thegn.desktop" "$tmp/dfv-desktop.before"
  dfv_manifest="$dfv_data/thegn/install-manifest"
  dfv_manifest_before="$(cksum "$dfv_manifest")"
  printf 'dfv new bytes\n' >"$install_repo/target/release/thegn"
  dfv_status=0
  if PATH="$fakebin:$PATH" HOME="$tmp/dfv-home" XDG_CONFIG_HOME="$tmp/dfv-config" \
    XDG_DATA_HOME="$dfv_data" TG_DFV_STATUS=1 TG_DFV_MESSAGE='error: fixture rejection' \
    "$install_repo/install.sh" "$dfv_bin" >"$tmp/dfv-refusal.out" 2>&1; then
    echo "desktop validator rejection unexpectedly succeeded" >&2
    exit 1
  else
    dfv_status=$?
  fi
  [[ $dfv_status -ne 0 && $(<"$tmp/dfv-refusal.out") == *"invalid desktop entry"* &&
  $(<"$tmp/dfv-refusal.out") == *"fixture rejection"* &&
  $(<"$tmp/dfv-refusal.out") == *"restored the previous installation"* ]] || {
    echo "desktop validator rejection did not visibly refuse and roll back" >&2
    cat "$tmp/dfv-refusal.out" >&2
    exit 1
  }
  if ! cmp -s "$tmp/dfv-thegn.before" "$dfv_bin/thegn" || ! cmp -s "$tmp/dfv-tg.before" "$dfv_bin/tg" ||
    ! cmp -s "$tmp/dfv-tg-tui.before" "$dfv_bin/tg-tui" ||
    ! cmp -s "$tmp/dfv-desktop.before" "$dfv_data/applications/thegn.desktop"; then
    echo "desktop validator refusal did not restore every prior artifact" >&2
    exit 1
  fi
  [[ $(cksum "$dfv_manifest") == "$dfv_manifest_before" ]] || {
    echo "desktop validator refusal changed the previous manifest" >&2
    exit 1
  }
  shopt -s nullglob
  dfv_leftovers=("$dfv_bin"/.thegn-install* "$dfv_data/applications"/.thegn-install*)
  shopt -u nullglob
  if ((${#dfv_leftovers[@]} > 0)); then
    echo "desktop validator refusal left staging or backup files" >&2
    printf '%s\n' "${dfv_leftovers[@]}" >&2
    exit 1
  fi
  printf '#!/usr/bin/env sh\nexit 0\n' >"$install_repo/target/release/thegn"
  chmod 0755 "$install_repo/target/release/thegn"

  # desktop-validate-structure: the extracted function rejects malformed entries.
  sed -n '/^validate_desktop_entry() {/,/^}/p' "$repo/install.sh" >"$tmp/vde.sh"
  [[ -s $tmp/vde.sh ]] || {
    echo "validate_desktop_entry function was not found for table tests" >&2
    exit 1
  }
  (
    PATH="$fakebin:$PATH"
    # shellcheck disable=SC1091 # This extracted function file is generated just above.
    source "$tmp/vde.sh"
    vde_file="$tmp/vde-fixture.desktop"
    vde_expect() {
      local fixture="$1" expected_status="$2" actual_status=0
      if validate_desktop_entry "$vde_file" "$tmp/bin/tg" >/dev/null 2>&1; then
        actual_status=0
      else
        actual_status=$?
      fi
      if [[ $expected_status == 0 && $actual_status -ne 0 ]] ||
        [[ $expected_status -ne 0 && $actual_status -eq 0 ]]; then
        echo "desktop structure fixture failed: $fixture" >&2
        return 1
      fi
    }
    cp "$desktop" "$vde_file"
    vde_expect generated-entry 0 || exit 1
    printf 'Type=Application\nName=thegn\nX-Thegn-Installer=true\nExec=%s --standalone\nTryExec=%s\nIcon=thegn\n' \
      "$tmp/bin/tg" "$tmp/bin/tg" >"$vde_file"
    vde_expect missing-header 1 || exit 1
    printf '[Desktop Entry]\nType=Application\nName=thegn\nX-Thegn-Installer=true\nExec=%s --standalone\nTryExec=%s\nIcon=thegn\n[Other]\n' \
      "$tmp/bin/tg" "$tmp/bin/tg" >"$vde_file"
    vde_expect second-group 1 || exit 1
    printf '[Desktop Entry]\nType=Application\nName=thegn\nName=duplicate\nX-Thegn-Installer=true\nExec=%s --standalone\nTryExec=%s\nIcon=thegn\n' \
      "$tmp/bin/tg" "$tmp/bin/tg" >"$vde_file"
    vde_expect duplicate-name 1 || exit 1
    printf '[Desktop Entry]\nType=Application\nName=thegn\nX-Thegn-Installer=true\nExec=/wrong/tg --standalone\nTryExec=%s\nIcon=thegn\n' \
      "$tmp/bin/tg" >"$vde_file"
    vde_expect wrong-exec 1 || exit 1
    printf '[Desktop Entry]\nType=Application\nName=thegn\nX-Thegn-Installer=true\nExec=%s --standalone\nIcon=thegn\n' \
      "$tmp/bin/tg" >"$vde_file"
    vde_expect missing-tryexec 1 || exit 1
    printf '[Desktop Entry]\nType=Application\nName=bad\001name\nX-Thegn-Installer=true\nExec=%s --standalone\nTryExec=%s\nIcon=thegn\n' \
      "$tmp/bin/tg" "$tmp/bin/tg" >"$vde_file"
    vde_expect control-character 1 || exit 1
    printf '[Desktop Entry]\nType=Application\nnot a key\nName=thegn\nX-Thegn-Installer=true\nExec=%s --standalone\nTryExec=%s\nIcon=thegn\n' \
      "$tmp/bin/tg" "$tmp/bin/tg" >"$vde_file"
    vde_expect malformed-key 1 || exit 1
  ) || exit 1

  # desktop-validate-real-tool: positive control when the host has the real validator.
  if command -v desktop-file-validate >/dev/null 2>&1; then
    if ! desktop-file-validate "$desktop"; then
      echo "desktop-file-validate rejected the generated entry" >&2
      exit 1
    fi
  else
    echo "desktop-file-validate not installed; real-tool check skipped"
  fi
fi

echo "install plan checks passed"
