#!/usr/bin/env bash
# Two distinct repos that share a basename (e.g. two `WASHU` checkouts) must get
# DISTINCT, stable slugs — otherwise their tabs collide in the one session and
# selecting one highlights/activates the other. Hermetic (no zellij needed).
# shellcheck disable=SC2015  # `cond && ok || fail` is intentional; ok never fails
set -u

SZD="$(cd "$(dirname "$0")/.." && pwd)"
SZ="$SZD/target/release/superzej"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
[ -x "$SZ" ] || {
	echo "FAIL: build first (cargo build --release)"
	exit 1
}

unset ZELLIJ ZELLIJ_SESSION_NAME ZELLIJ_PANE_ID
export HOME="$TMP" XDG_STATE_HOME="$TMP/state" XDG_CONFIG_HOME="$TMP/config"
export SUPERZEJ_NO_EXEC=1 GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
mkdir -p "$XDG_CONFIG_HOME/superzej"
printf 'repo_roots = ["%s"]\n' "$TMP" >"$XDG_CONFIG_HOME/superzej/config.toml"

A="$TMP/x/WASHU"
B="$TMP/y/WASHU" # same basename, different dirs
for d in "$A" "$B"; do
	mkdir -p "$d"
	git -C "$d" init -q
	git -C "$d" commit -q --allow-empty -m init
done

"$SZ" new-workspace "$A" >/dev/null 2>&1
"$SZ" new-workspace "$B" >/dev/null 2>&1

slugs="$("$SZ" workspaces 2>/dev/null | grep -E "/WASHU$" | cut -f1)"
uniq_n="$(echo "$slugs" | sort -u | grep -c .)"
echo "assigned slugs: $slugs"

# stability: same slugs on a second read
slugs2="$("$SZ" workspaces 2>/dev/null | grep -E "/WASHU$" | cut -f1)"

fail=0
if [ "$uniq_n" -eq 2 ]; then echo "  ✓ same-basename repos get distinct slugs"; else
	echo "  ✗ slugs collide: $slugs"
	fail=1
fi
if [ "$(echo "$slugs" | sort)" = "$(echo "$slugs2" | sort)" ]; then echo "  ✓ slugs are stable across calls"; else
	echo "  ✗ slugs not stable"
	fail=1
fi

echo
[ "$fail" = 0 ] && echo PASS || {
	echo FAIL
	exit 1
}
