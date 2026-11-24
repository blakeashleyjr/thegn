# Design

`Feature` gains nine variants in `crates/thegn-core/src/channel.rs`, each with a stable id, all with
`Stability::Experimental`, so `allowed_in(Channel::Stable)` is false for each. `Feature::ALL` lists the
existing seven first and then the nine in the order above, for a length of 16. A `Feature::PROGRAMME` array
holds the nine so `thegn doctor` can append "experimental, not yet implemented" to their rows.

`Config::clamp_to_channel` gets one arm per new feature returning `false` ("nothing configurable yet; the
owner item fills the arm"), so the stable clamp output of every existing config is unchanged. The host's
clamped-feature bitset widens from 8 to 32 bits, because 16 features do not fit in eight. Existing feature
bit assignments remain in place and the new features append after them. Doctor's text
table widens its feature-id column, and its JSON gains nine keys from `Feature::ALL`.

Nothing here adds a thread, process, wake source, catalog row, config key or schema change. Doctor prints nine
more rows of formatted strings only.

## Why one change for nine features

Each owning workstream would otherwise add its own variant, editing the same enum, the same `ALL` array and
the same clamp match in parallel, and each would restate an `ALL` length. Adding them together removes that
collision. Dropped names: `SandboxRecipes` (recipes are named backends, so `SandboxBackends`) and
`EgressPresets` (presets need the proxy, so `EgressProxy`). Connectors, mux, remote daemon, thin client, sync
and the WSL connector stay under the existing `Remote`; config providers and plugin env providers stay under
`Providers`.

## Why an ADDED requirement

The `release-channels` spec states the gated set as a list inside one requirement, and the nine features are
not yet gated by any config key. Rewriting that requirement as a MODIFIED block would collide with any other
change that touches it and would claim a gating behaviour that does not exist. A separate ADDED requirement
records only the fact that matters now: the nine ids exist, are experimental and are disallowed on stable.
