## Why

The background LOC and size scanners never went idle on a large registry, and the state database needed a manual `thegn db vacuum` to stay small. Both should settle on their own.

## What Changes

- Background lanes gate on a next-due time and sleep between sweeps; background LOC is recounted only when git state moved.
- The UI maintains the state database itself: conversion to incremental auto-vacuum, incremental page release, bounded retention and a state-file sweep, behind one lock.
- The planner simulation is the idle regression gate.
- No config key or default changes; no schema change.

## Impact

- Specs: state-db, disk-lifecycle, perf-suite.
- Docs: configuration, bars, cli, debugging help pages.
