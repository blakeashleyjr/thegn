#!/bin/sh
# Pane "shell" shared by every tool in the comparison harness (set as $SHELL).
# Prints READY, optionally runs the workload in $TG_CMP_CMD, prints DONE, then
# parks on a uniquely numbered `sleep` so the harness can count live panes.
# Identical for tmux, zellij and thegn, so the only variable is the multiplexer.
printf '%s\n' "$TG_CMP_READY"
if [ -n "${TG_CMP_CMD:-}" ]; then
  # A multiplexer renders screen state, not a byte stream: READY would scroll away
  # unseen if the workload started at once. Hold until the harness saw READY.
  while [ ! -e "$TG_CMP_GO" ]; do sleep 0.01; done
  eval "$TG_CMP_CMD" >&1
fi
printf '%s\n' "$TG_CMP_DONE"
exec sleep "$TG_CMP_SLEEP"
