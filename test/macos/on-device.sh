#!/usr/bin/env bash
# Run the checked-in Mac OCI evidence suite from a private output directory.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [ "$#" -ne 1 ] || [ -z "$1" ]; then
  echo "usage: $0 PRIVATE_OUTDIR" >&2
  exit 2
fi
if [ "$(uname -s)" != Darwin ]; then
  echo "macOS on-device evidence requires Darwin" >&2
  exit 2
fi
exec "$root/scripts/spikes/ws05-p0-01.sh" "$1"
