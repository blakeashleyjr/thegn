# How to read this changelog

The changelog records notable changes in each release, following [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/). Sections group changes by
what happened:

- **Added**: new user-visible capabilities.
- **Changed**: updates to existing behavior.
- **Fixed**: corrected behavior.
- **Security**: changes that address security issues.
- **Removed**: capabilities or behavior that are no longer available.

A `(dev channel)` or `(Dev channel)` marker on a bullet shows a change visible
only on the experimental dev channel. The stable and dev channels are
described in the [release channel guide](help/release-channels.md).

Alpha builds migrate their local database forward as needed. An older build
refuses to open a database written by a newer build, so update the application
before opening that database.

## Writing an entry

Use a heading in this form, with no process labels:

`## [0.1.0-alpha.N] — YYYY-MM-DD`

Write one bullet for each user-visible change, describing the outcome for the
person using the product. Mark a bullet with `(Dev channel)` when the change
is visible only on that channel. Do not include issue ids, internal batch,
lane, or slug names, or performance figures stated as measured.

For example:

## [0.1.0-alpha.56] — 2026-11-06

- The changelog checker flags internal process labels in new release headings.
