#!/usr/bin/env sh
# The smallest thegn plugin: a one-shot statusbar segment.
#
# Declared by a [[plugins]] entry or a plugins/<dir>/plugin.toml manifest, run
# by the host on its cadence via NDJSON: every line printed to stdout is one
# JSON message. `register` claims the contribution declared in the manifest;
# `update` fills its statusbar surface with a view. Anything that is not JSON
# (like a stray echo) is kept by the host as junk for diagnostics — try it.
#
# Wire reference: openspec/specs/plugin-api + docs/extending/plugin.md.

printf '%s\n' '{"method":"register","params":{"plugin":"hello","contribution":{"id":"hello.seg","extension_point":"StatusBarSegment","label":"Hello","surface":"hello.segment"}}}'
printf '%s\n' '{"method":"update","params":{"surface":"hello.segment","view":{"spans":[{"text":"hello from a plugin","role":"Accent"}]}}}'
