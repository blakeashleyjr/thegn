# Design — resident plugin app tabs

## Re-land rather than merge

The `tg/sturdy-pearl` proof of concept is a reference for the app contract and
bounded UI, not a base to merge. Main has since rebuilt the resident plugin
runtime and carries the security and lifecycle fixes this change must preserve.

## Use main's resident lifecycle

Main's SessionGen, bounded inbox, and cancelable respawn timer replace the
branch's mailbox, generation counter, and start barrier. The plugin publishes
events only after `activate`; the loop sends `activate` only after installing
the writer, so stdout needs no start barrier. One request-id allocator per
session is shared by app events and `provider.call`.

## Focus and input ownership

Focus tickets are single-use, expire after five seconds, and must match the live
SessionGen, surface, and navigation epoch. A ticket is refused while a host
modal owns input. App keys are keymap actions, never hard-coded chords.
Essential-chord pre-emption applies only to plugin app tabs; built-in Observe is
unchanged. This is the planner default pending owner review.

## Gate and idle behavior

App tabs are experimental behind `plugin_apps`. An app with no pending request
has no timer: a timeout exists only while a request is pending.

## Risks

Input routing changes inside plugin app tabs, and Plugin API's additive minor
version bump to 0.4.0, need focused review and compatibility coverage.
