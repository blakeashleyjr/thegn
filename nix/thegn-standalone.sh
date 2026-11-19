#!/bin/sh
# Opens thegn in its own Alacritty window (the app-launcher entry).
if ! command -v alacritty >/dev/null 2>&1; then
  echo "@bin@-standalone: alacritty not found; run @bin@ in a terminal" >&2
  exit 127
fi
exec alacritty --class @bin@ --title @bin@ --config-file "@out@/share/thegn/alacritty.toml" -e "@out@/bin/@bin@" "$@"
