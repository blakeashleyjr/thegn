#!/usr/bin/env bash
set -euo pipefail

# Runs generator logic on Linux through PATH shims; no macOS platform seam is needed.
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf -- "$tmp"' EXIT
mac_bin="$tmp/mac-bin"
mkdir -p "$mac_bin" "$tmp/home"

cat >"$mac_bin/uname" <<'EOF'
#!/usr/bin/env bash
printf 'Darwin\n'
EOF
cat >"$mac_bin/thegn" <<'EOF'
#!/usr/bin/env bash
printf 'thegn 1.2.3\n'
EOF
cat >"$mac_bin/plutil" <<'EOF'
#!/usr/bin/env bash
exit "${FAKE_PLUTIL_STATUS:-0}"
EOF
chmod 0755 "$mac_bin/uname" "$mac_bin/thegn" "$mac_bin/plutil"

run_make_app() {
  PATH="$mac_bin:$PATH" HOME="$tmp/home" FAKE_PLUTIL_STATUS="${FAKE_PLUTIL_STATUS:-0}" \
    "$repo/packaging/macos/make-app.sh" --bin "$mac_bin/thegn" "$@"
}

assert_no_workdirs() {
  local dest="$1"
  if find "$dest" -mindepth 1 -maxdepth 1 \
    \( -name '.thegn.app.staging.*' -o -name '.thegn.app.old.*' \) -print -quit | grep -q .; then
    echo "make-app test: unexpected staging/old directory in $dest" >&2
    return 1
  fi
}

fail() {
  echo "make-app test: $1" >&2
  exit 1
}

# fresh bundle
apps="$tmp/apps"
run_make_app --dest "$apps" >/dev/null || fail "fresh bundle generation failed"
bundle="$apps/thegn.app"
for required in \
  "$bundle/Contents/MacOS/thegn" \
  "$bundle/Contents/Resources/common.sh" \
  "$bundle/Contents/Resources/thegn.command" \
  "$bundle/Contents/Resources/.thegn-make-app" \
  "$bundle/Contents/Info.plist" \
  "$bundle/Contents/PkgInfo"; do
  [[ -f $required ]] || fail "fresh bundle missing $required"
done
[[ -x "$bundle/Contents/MacOS/thegn" ]] || fail "fresh bundle executable is not executable"
assert_no_workdirs "$apps" || fail "fresh bundle left temporary directories"

# regeneration replaces the old bundle
printf stale >"$bundle/Contents/Resources/stale-file"
run_make_app --dest "$apps" >/dev/null || fail "bundle regeneration failed"
[[ ! -e "$bundle/Contents/Resources/stale-file" ]] || fail "regeneration retained stale files"
[[ -f "$bundle/Contents/Resources/.thegn-make-app" ]] || fail "regeneration omitted ownership marker"
assert_no_workdirs "$apps" || fail "regeneration left temporary directories"

# legacy bundle is adopted
legacy="$tmp/legacy/thegn.app"
mkdir -p "$legacy/Contents/Resources"
printf legacy >"$legacy/Contents/Resources/common.sh"
mkdir -p "$legacy/Contents"
printf '<string>com.github.blakeashleyjr.thegn</string>\n' >"$legacy/Contents/Info.plist"
run_make_app --dest "$tmp/legacy" >/dev/null || fail "legacy bundle was not adopted"
[[ -f "$legacy/Contents/Resources/.thegn-make-app" ]] || fail "adopted bundle lacks ownership marker"
assert_no_workdirs "$tmp/legacy" || fail "legacy adoption left temporary directories"

# foreign bundle is refused without modification
foreign="$tmp/foreign/thegn.app"
mkdir -p "$foreign/Contents"
printf '<string>com.example.foreign</string>\n' >"$foreign/Contents/Info.plist"
cp "$foreign/Contents/Info.plist" "$tmp/foreign.plist.before"
if run_make_app --dest "$tmp/foreign" >"$tmp/foreign.out" 2>"$tmp/foreign.err"; then
  fail "foreign bundle was accepted"
fi
grep -Fq "$foreign" "$tmp/foreign.err" || fail "foreign refusal did not name the path"
cmp -s "$tmp/foreign.plist.before" "$foreign/Contents/Info.plist" || fail "foreign bundle was modified"
assert_no_workdirs "$tmp/foreign" || fail "foreign refusal left staging directories"

# failed lint keeps the previous bundle
cp "$bundle/Contents/Resources/common.sh" "$tmp/common.sh.before"
if FAKE_PLUTIL_STATUS=1 run_make_app --dest "$apps" --env THEGN_LOG=changed >"$tmp/lint.out" 2>"$tmp/lint.err"; then
  fail "failed plist lint was accepted"
fi
cmp -s "$tmp/common.sh.before" "$bundle/Contents/Resources/common.sh" || fail "failed lint changed the previous bundle"
assert_no_workdirs "$apps" || fail "failed lint left staging/old directories"

# symlinked bundle path is refused
mkdir -p "$tmp/target-bundle"
printf untouched >"$tmp/target-bundle/sentinel"
mkdir -p "$tmp/apps2"
ln -s "$tmp/target-bundle" "$tmp/apps2/thegn.app"
if run_make_app --dest "$tmp/apps2" >"$tmp/symlink-app.out" 2>"$tmp/symlink-app.err"; then
  fail "symlinked app path was accepted"
fi
[[ -f "$tmp/target-bundle/sentinel" ]] || fail "symlink target was modified"
[[ -L "$tmp/apps2/thegn.app" ]] || fail "symlinked app path was replaced"
assert_no_workdirs "$tmp/apps2" || fail "symlinked app refusal left staging directories"

# symlinked destination is refused
mkdir -p "$tmp/target-dest"
ln -s "$tmp/target-dest" "$tmp/symlink-dest"
if run_make_app --dest "$tmp/symlink-dest" >"$tmp/symlink-dest.out" 2>"$tmp/symlink-dest.err"; then
  fail "symlinked destination was accepted"
fi
[[ -z "$(find "$tmp/target-dest" -mindepth 1 -print -quit)" ]] || fail "symlink destination target received files"

# dry-run writes nothing
dry_dest="$tmp/dry-run-apps"
run_make_app --dest "$dry_dest" --dry-run >"$tmp/dry-run.out" || fail "dry-run failed"
[[ ! -e $dry_dest ]] || fail "dry-run created its destination"

echo "make-app tests passed"
