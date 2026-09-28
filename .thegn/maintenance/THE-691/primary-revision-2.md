# Primary review — THE-691 revision 2 (reviewing row 672)

The bounded tolerant reader is right and the profile-layer question is resolved in
your diff. One gate failure remains, and it is a ratchet rather than logic.

## The failure

```
FAIL thegn-core platform_ratchet_tests::platform_cfgs_are_pinned
```

`crates/thegn-core/src/config.rs` now carries a platform `#[cfg]` — the
`config_source_kind` helper that names a FIFO / socket / device / directory — and
`config.rs` is **not** on `test/platform-cfg-core-ratchet.txt`. That list is
shrink-only, so a new platform `#[cfg]` in an unlisted file fails the build. **Do
not add `config.rs` to the allowlist**: it is one of this repo's god-files and
listing it would license every future `#[cfg]` there.

## Move `config_source_kind` to `crates/thegn-core/src/util.rs`

`util.rs` is **already pinned** on that ratchet, so a per-OS branch inside it needs
no new entry. A "what kind of filesystem object is this" namer is an ordinary
utility, and `config.rs` should call `util::config_source_kind(&metadata)` — or
whatever you name it — with no `#[cfg]` of its own.

**Not `fsperm.rs`**, even though it is also allowlisted and is arguably the better
domain fit: THE-455 is concurrently adding a no-follow open there in this same
batch, and two live lanes editing one file is a conflict I would rather not create.
If a later change wants to gather all the filesystem-fact helpers in `fsperm`, that
is a tidy-up for after both have landed.

## Keep everything else

The bounded reader with its metadata check, size check and `take(limit + 1)` so a
file that grows after its metadata was observed is still refused; both refusals
naming the observation rather than a category; tolerance preserved for a malformed
**regular** file; admission untouched; and the profile-layer coverage — including
whatever you determined about how the profile path resolves under the fixture's
`env_clear()`. **State that determination in your report** if you have not already:
if the preflight and the loader can ever resolve different paths, that is a guard
which does not guard, and it matters more than this ratchet.

## Validation

`nix develop --command cargo nextest run -p thegn-core -p thegn-host` — the **whole**
crates. A scoped `-E` filter is what let this reach the gate; the ratchet tests are
never in a filter aimed at the feature. **The pipeline sandbox mounts `/nix/store`
read-only, so this usually fails outright** — say exactly that and stop if it does;
a supervisor runs the crate-wide verification centrally now.

Never report a verdict for code you could not compile; state what you could not run.
