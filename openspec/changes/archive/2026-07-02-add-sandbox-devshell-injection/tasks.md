# Tasks

## 1. Resolver (superzej-core)

- [ ] 1.1 Add `devenv.rs`: parse `nix print-dev-env --json` (exported-var
      extraction; malformed/empty input) — **unit tests** (95% gate).
- [ ] 1.2 Cache-key derivation from `flake.lock` + `flake.nix` hash + invalidation
      — **unit tests**.
- [ ] 1.3 Config keys `[sandbox] inject_devshell` (default true), `[sandbox]
nix_daemon` (default false) in `config.rs`.

## 2. Off-loop resolve + inject (superzej-host)

- [ ] 2.1 Background-thread resolve (waker pulse, cache write); startup prewarm of
      the active workspace; assert no loop blocking / no polling timeout.
- [ ] 2.2 Pane-spawn env merge host-side before the sandbox exec (PATH prepended,
      other vars set-if-unset).

## 3. Tier B daemon (superzej-core sandbox.rs)

- [ ] 3.1 `nix_daemon` mount of the daemon socket + `NIX_REMOTE=daemon`; host
      precondition check (warn + stay off when no socket).

## 4. Onboarding

- [ ] 4.1 `just doctor` recipe + friendlier tool-missing failures in `lint`/`fmt`;
      committed `.envrc` (`use flake`); document keys in `config.toml.example`.

## 5. Validate

- [ ] 5.1 Smoke: flake repo populates the injected PATH; non-flake is a no-op.
- [ ] 5.2 Run `just ci` (includes `openspec-validate`).
