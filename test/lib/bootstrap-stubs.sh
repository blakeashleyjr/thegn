#!/usr/bin/env bash
# Shared stubs for the bootstrap-script tests (test/ci-runner-setup-test.sh and
# test/setup-macos-test.sh). Both tests export TEST_ROOT and STUB_BIN and put
# STUB_BIN first on PATH, so nothing here reaches the network or sudo.

# curl stand-in: copy the fixture payload to the -o destination, report
# $TEST_EFFECTIVE_URL as the resolved origin (default: the requested URL), and
# simulate a failed transfer with TEST_CURL_FAIL=1. Every invocation is recorded
# in $TEST_ROOT/curl-argv so tests can assert the transfer constraints.
bootstrap_stub_curl() {
  cat >"$STUB_BIN/curl" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$TEST_ROOT/curl-argv"
destination=''
write_out=''
url=''
while (($#)); do
  case "$1" in
  -o)
    destination="$2"
    shift 2
    ;;
  -w | --write-out)
    write_out="$2"
    shift 2
    ;;
  -*)
    shift
    ;;
  *)
    url="$1"
    shift
    ;;
  esac
done
[ -n "$destination" ] || exit 93
if [ "${TEST_CURL_FAIL:-0}" = 1 ]; then
  printf 'curl: (22) simulated transfer failure\n' >&2
  exit 22
fi
cp -- "$TEST_PAYLOAD" "$destination" || exit 23
[ -n "$write_out" ] && printf '%s' "${TEST_EFFECTIVE_URL:-$url}"
exit 0
STUB
  chmod 700 "$STUB_BIN/curl"
}

# uname stand-in so the macOS bootstrap can be exercised on any host.
bootstrap_stub_uname() {
  cat >"$STUB_BIN/uname" <<'STUB'
#!/usr/bin/env bash
case "${1:-}" in
-m) printf '%s\n' "${TEST_ARCH:-arm64}" ;;
*) printf '%s\n' "${TEST_KERNEL:-Darwin}" ;;
esac
STUB
  chmod 700 "$STUB_BIN/uname"
}
