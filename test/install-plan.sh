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

# install.sh copies the built binary, so the stub above lives at the expected
# path in the throwaway repo. The fake cargo builds nothing.
cleanup() {
  rm -rf "$tmp"
  return 0
}
trap cleanup EXIT

install_out="$(PATH="$fakebin:$PATH" HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config" XDG_DATA_HOME="$tmp/data" "$install_repo/install.sh" "$tmp/bin")"
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

echo "install plan checks passed"
