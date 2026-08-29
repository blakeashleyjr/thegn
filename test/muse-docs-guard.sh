#!/usr/bin/env bash
# Keep the hand-run Muse setup hermetic in the same ways as just e2e.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

files=(docs/testing-with-muse.md extensions/skills/tui-check/SKILL.md)
t_prefix='$'
required=(
  "printf '[user]\\nname = muse\\nemail = muse@example.invalid\\n' > \"\$T/gitconfig\""
  "--env GIT_CONFIG_GLOBAL=\"${t_prefix}T/gitconfig\""
  '--env GIT_CONFIG_SYSTEM=/dev/null'
  '--env DBUS_SESSION_BUS_ADDRESS="unix:path=/dev/null/e2e-no-dbus"'
)

for file in "${files[@]}"; do
  for token in "${required[@]}"; do
    if ! grep -Fq -- "$token" "$file"; then
      echo "muse-docs-guard: $file is missing $token" >&2
      exit 1
    fi
  done
done

echo "muse-docs-guard: clean"
