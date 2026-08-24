## Why

Audit rows A3/A5: after the forge, the remaining seams still carried the old idioms — `CiProvider` was `async fn` in trait dispatched through a hand-written `CiClient` enum (every host caller wrapped a subprocess in a throwaway runtime); the managed-provider vocabulary lived in five hand-synced string lists across core and svc; `sandbox::Backend` re-derived label/binary/family in eleven-arm matches across eight files and kept its own alias table beside `config_enum! SandboxBackend`; the theme preset list had no reverse check; and `thegn-svc/src/ssh.rs` (`RemoteExec`/`CliSsh`) had zero callers.

## What Changes

- **CI is a sync, object-safe seam**: `CiProvider: Probe` with plain `&self` methods and `fn system()`; `CiClient` is `Box<dyn CiProvider>`; `client_for_system` returns the box; host callers lose their `block_on` wrappers; `async-trait` ratchet 10 → 9.
- **One provider vocabulary**: `config_enum! EnvProviderKind` (`custom|""`, daytona, sprites, hetzner, digitalocean, fly, machine0) with `is_vps / exec_api / ssh_reached / scale_to_zero / self_suspends`; the five predicate fns and `thegn_svc::provider::exec_api_by_name` / `VpsKind::parse` delegate to it; `provider_factory::provider_for_named` matches it exhaustively (a new kind without a factory arm is a compile error). `EnvProviderConfig.provider` stays a `String` (config shape unchanged).
- **Sandbox `BackendProfile`**: one `Backend::profile()` table (label, binary, family, rootful); `label/binary/is_oci/is_host_toolchain` derive from it; `backend_enter_argv` and the OCI-runtime loops key on `BackendFamily` / `Backend::oci_runtimes()`; `Backend::parse` delegates to the `config_enum!` alias table (reserved `wsl` parses to `None`); `Backend::ALL` + a table test.
- **Theme**: `every_preset_arm_is_listed` (source scan) closes the reverse direction of the `PRESETS` ⇔ `preset()` pair.
- **Deleted** `crates/thegn-svc/src/ssh.rs` (`RemoteExec`, `CliSsh`, `config_forces_cli`) — no callers; ssh reach is `thegn_core::remote::ssh_base` + the host bridges.

## Capabilities

### Modified Capabilities

- `ci-inspection`: the provider trait is sync/object-safe; selection returns a boxed provider.
- `sandbox`: backends are described by one profile table keyed by family; the config enum is the only alias table.
- `provider-seams`: the managed-provider kind vocabulary is a `config_enum!` with behaviour predicates as methods.

## Impact

`crates/thegn-svc/src/{ci.rs,provider.rs,vps/mod.rs,seam/registry.rs,lib.rs}`, `crates/thegn-core/src/{config_env_tables.rs,config.rs,sandbox.rs,theme.rs}`, `crates/thegn-host/src/{provider_factory.rs,actions.rs,ci_refresh.rs,cmd/ci.rs}`, `test/async-trait-ratchet.txt`. No config-shape, schema or render change. Roadmap: A3/A5; AV (CI seam shape), AS/AT.
