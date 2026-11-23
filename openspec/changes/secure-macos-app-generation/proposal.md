# Secure macOS app bundle generation

## Why

The app generator validates hostile inputs and stages bundles, but shell path
lookups leave a parent-symlink race and replacement uses two renames with a
period where the app path is absent. Harden that existing installer-owned
transaction so an unsafe or unavailable filesystem primitive fails closed.

## What changes

- Pin the canonical destination directory before bundle filesystem effects.
- Revalidate existing installer ownership at promotion time.
- Use the host's atomic directory exchange for upgrades and an atomic
  no-replace rename for first installation.
- Keep partial-generation cleanup restricted to a marked bundle created by
  this generator.

## Out of scope

No changes to launcher behavior, bundle contents, signing, notarization, or
release artifact distribution.
