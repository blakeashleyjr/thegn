# Update availability notice

## Problem

Stable builds do not tell users when a newer release is available. Users should
see a quiet, dismissible notice without making launch or rendering depend on
network access.

## Scope

Check the latest public GitHub release at most once per day, cache the result in
the XDG state directory, expose an opt-out, and show an install-specific upgrade
command in the statusbar and `thegn doctor`. Dev builds skip the check.
