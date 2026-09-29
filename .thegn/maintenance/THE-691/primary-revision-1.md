# Primary review — THE-691 revision 1 (reviewing row 667)

**The implementation is right and the primary verified it: 155/156.** The one
failure is your own new profile-layer test, and it is worth keeping.

`read_tolerant_config_source` is well built — metadata check, size check, and
`take(limit + 1)` so a file that **grows after its metadata was observed** is still
refused. That last one I did not ask for and it is the case a naive size check
misses. The error text names the observation (`config source is a FIFO, not a
regular file`) rather than a category, which is exactly right.

## The one failure

```
FAIL unix::tolerant_inspection_refuses_nonregular_profile_configuration
  static_cli_support/unix.rs:107  assert!(!output.status.success(), "accepted profile FIFO")
```

The command **succeeded** with a FIFO at the profile overlay path, so the profile
layer was never inspected.

## What the primary already ruled out

I traced this rather than guessing, so start from here:

- **The preflight is wired for both commands.** `main.rs:1193` calls
  `check_layered_source_files` for exactly `Config::Get` and `Automations::Test`.
- **The preflight checks both layers.** `check_layered_source_files` reads the base
  with `?` and then `profile_overlay_path(env)` with `?` — so a profile refusal
  would propagate.
- **The fixture does select the profile.** `Fixture::command` sets
  `THEGN_PROFILE=fixture-profile` when `profile_env` is true, and your test passes
  `true`.
- **The test creates the file where the layout implies**:
  `<XDG_CONFIG_HOME>/thegn/profiles/fixture-profile/config.toml`, and the fixture
  sets `XDG_CONFIG_HOME` (and `APPDATA`) to its own `config` dir.

So every link looks correct, which means the break is in **how the profile path
resolves inside the fixture's environment** — most likely
`util::xdg_config_home()` not returning the fixture's config dir under
`env_clear()`. Note the comment at the profile-overlay read site says the profile
comes from the **REAL** config home and that `XDG_CONFIG_HOME` is "deliberately NOT
rerooted"; if that means the profile layer intentionally ignores a rerooted config
home, then the _test_ is asserting something the design forbids and the fixture
needs a different way to place the profile.

**Determine which it is and say so explicitly**:

- If `xdg_config_home()` should honour the fixture's env and does not, that is the
  bug — fix it or route the preflight through the same resolution the loader uses,
  so preflight and load cannot disagree about which file they mean. **That
  disagreement is the more serious possibility**: a preflight checking a different
  path than the loader reads is a guard that does not guard.
- If the profile layer deliberately reads the non-rerooted home, then the guard is
  already correct and the **test** must place the FIFO where that resolution
  actually points. Say that plainly rather than deleting the test.

Either way, do not weaken the assertion to make it pass, and do not drop the
profile case: an unbounded read on the profile layer hangs exactly as the base one
did.

## Confirmed — keep as implemented

The bounded tolerant reader and its two refusals; tolerance preserved for a
malformed **regular** file (your
`config_get_still_inspects_malformed_regular_configuration` is the test that proves
the contract still holds); the oversized-source refusal; admission left untouched.

## Validation

`nix develop --command cargo nextest run -p thegn-host -E 'test(tolerant) +
test(static_cli)'` and `cargo check -p thegn-host --all-targets`. **The pipeline
sandbox mounts `/nix/store` read-only, so this usually fails outright** — say
exactly that and stop if it does. The primary re-runs it regardless.

Never report a verdict for code you could not compile; state what you could not run.
