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
cat >"$mac_bin/envdump" <<'EOF'
#!/usr/bin/env bash
if [[ ${1-} == --version ]]; then
  printf 'thegn 1.2.3\n'
else
  printf '%s' "$TG_PROBE" >"$ENVDUMP_OUT"
  printf '%s' "${TG_EMPTY-unset}" >"$ENVDUMP_OUT.empty"
fi
EOF
chmod 0755 "$mac_bin/uname" "$mac_bin/thegn" "$mac_bin/plutil" "$mac_bin/envdump"

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
  "$bundle/Contents/Resources/env" \
  "$bundle/Contents/Resources/launch.sh" \
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
cp "$bundle/Contents/Resources/env" "$tmp/env.before"
if FAKE_PLUTIL_STATUS=1 run_make_app --dest "$apps" --env THEGN_LOG=changed >"$tmp/lint.out" 2>"$tmp/lint.err"; then
  fail "failed plist lint was accepted"
fi
cmp -s "$tmp/common.sh.before" "$bundle/Contents/Resources/common.sh" || fail "failed lint changed the previous bundle"
cmp -s "$tmp/env.before" "$bundle/Contents/Resources/env" || fail "failed lint changed the previous env data"
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

# Environment values stay data through the launcher, including shell syntax and newlines.
probe=$'a;b $(touch '"$tmp"$'/pwned) `touch '"$tmp"$'/pwned2` \'q\' "d" \\ %s\nline2 *?[x]'
test_user="${USER:-tester}"
probe_apps="$tmp/probe-apps"
run_make_app --bin "$mac_bin/envdump" --dest "$probe_apps" --env "TG_PROBE=$probe" --env TG_EMPTY= >/dev/null ||
  fail "hostile environment bundle generation failed"
probe_bundle="$probe_apps/thegn.app"
ENVDUMP_OUT="$tmp/envdump.out" USER="$test_user" bash "$probe_bundle/Contents/Resources/thegn.command" || fail "environment launcher failed"
printf '%s' "$probe" >"$tmp/probe.expected"
cmp -s "$tmp/probe.expected" "$tmp/envdump.out" || fail "environment value changed in transit"
[[ ! -s "$tmp/envdump.out.empty" ]] || fail "empty environment value was not preserved"
[[ ! -e "$tmp/pwned" && ! -e "$tmp/pwned2" ]] || fail "environment value executed shell syntax"
if grep -R -q 'eval' "$probe_bundle/Contents"; then
  fail "generated bundle contains eval"
fi
[[ -f "$probe_bundle/Contents/Resources/env" ]] || fail "generated bundle lacks env data"
[[ -x "$probe_bundle/Contents/Resources/launch.sh" ]] || fail "generated bundle launcher is not executable"
if grep -Fq 'TG_PROBE' "$probe_bundle/Contents/Resources/common.sh"; then
  fail "environment value leaked into common.sh"
fi
# shellcheck source=/dev/null
command_text="$(
  export USER="$test_user"
  . "$probe_bundle/Contents/Resources/common.sh"
  thegn_command /x/thegn /x/ghostty
)"
if [[ $command_text == *TG_PROBE* || $command_text == *pwned* ]]; then
  fail "terminal command contains an environment value"
fi

# A tampered key record refuses before the target process can run.
tampered_bundle="$tmp/tampered.app"
cp -R "$probe_bundle" "$tampered_bundle"
printf 'BAD;KEY=x\0' >>"$tampered_bundle/Contents/Resources/env"
if ENVDUMP_OUT="$tmp/tampered.out" USER="$test_user" bash "$tampered_bundle/Contents/Resources/thegn.command" >"$tmp/tampered.stdout" 2>"$tmp/tampered.stderr"; then
  fail "tampered environment key was accepted"
fi
[[ ! -e "$tmp/tampered.out" ]] || fail "tampered environment reached the target process"

# A record without '=' is malformed even though its text resembles a valid key.
no_value_bundle="$tmp/no-value.app"
cp -R "$probe_bundle" "$no_value_bundle"
printf 'FOO\0' >>"$no_value_bundle/Contents/Resources/env"
if ENVDUMP_OUT="$tmp/no-value.out" USER="$test_user" bash "$no_value_bundle/Contents/Resources/thegn.command" >"$tmp/no-value.stdout" 2>"$tmp/no-value.stderr"; then
  fail "environment record without '=' was accepted"
fi
[[ ! -e "$tmp/no-value.out" ]] || fail "malformed environment record reached the target process"

echo "make-app tests passed"
