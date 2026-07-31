# Tasks — release channels (stable / dev)

## 1. Capability registry (thegn-core)

- [x] 1.1 `channel.rs`: `Channel { Stable, Dev }`, `Stability`, `Feature`
      (`Remote`/`Providers`/`Ai`/`Observe`/`Placement`/`Trackers`), pure
      `stability`/`allowed_in` + `Channel::parse`; exported from `lib.rs`.
- [x] 1.2 Unit tests: experimental set is dev-only, `ALL` exhaustive, ids
      unique, parse aliases (`release`→stable, `experimental`→dev).

## 2. Config clamp (thegn-core)

- [x] 2.1 `Config::clamp_to_channel(channel) -> Vec<Feature>`: forces the
      disallowed masters off (`llm_proxy`, `observe`, `placement`,
      `sandbox.remote.host`, `[host.*]`) and drops non-GitHub `[issues]`
      providers/accounts; keeps GitHub Issues and the `[[agents]]` launcher.
- [x] 2.2 Tests: stable clamps all six + returns them; dev is a no-op.

## 3. Host resolution + wiring

- [x] 3.1 `channel_state.rs`: atomic holder, `resolve(env)` (`THEGN_CHANNEL` →
      `dev` feature default), `resolve_and_install`, `current`, `allows` + tests.
- [x] 3.2 `run.rs`: resolve + install + clamp after `load_layered`; log +
      `model.status` note when a clamp fired.
- [x] 3.3 `main.rs`: resolve/install/clamp in `run_subcommand` and the `open`
      path; refuse experimental verbs (`proxy`/`agent`/`host`/`placement`/`kaneo`)
      in the stable build with a dev-channel pointer.

## 4. Surfacing

- [x] 4.1 `thegn doctor`: "Release channel" section (human) + `channel` object
      (`--json`) with the per-feature allow table.
- [x] 4.2 Config-gated UI (Observe app tab, …) hides for free via the clamp.

## 5. Packaging + docs

- [x] 5.1 `thegn-host` `dev` Cargo feature (empty; flips the default channel).
- [x] 5.2 Nix: `package.nix` `channel` arg (`thegn-dev`/`tg-dev`, `buildFeatures`);
      flake `packages.dev` / `packages.thegn-dev`.
- [x] 5.3 justfile: dev-channel check folded into `build` (so `just ci` covers
      it); `just start-dev`.
- [x] 5.4 `config.toml.example`: documented "Release channel" header note.

## 6. Validation

- [x] 6.1 `cargo test -p thegn-core` (channel + clamp) and `-p thegn-host`
      (channel_state + doctor) green; clippy `-D warnings` clean incl. `--features dev`.
- [x] 6.2 Manual: `doctor` shows stable-denies-all / dev-allows-all; env override
      flips either binary; a config with experimental keys is clamped in stable
      and honoured in dev; an experimental verb is refused in stable.
- [ ] 6.3 Pre-PR gate: `just ci` (incl. `openspec validate --all --strict`).
