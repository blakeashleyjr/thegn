## 1. CI

- [x] 1.1 `CiProvider` sync + `Probe` + `system()`; `CiClient = Box<dyn>`; host callers de-wrapped

## 2. Providers

- [x] 2.1 `EnvProviderKind` + predicate methods; core predicates + svc `exec_api_by_name`/`VpsKind::parse` delegate; exhaustive factory match

## 3. Sandbox

- [x] 3.1 `BackendProfile`/`BackendFamily`/`Backend::ALL`/`oci_runtimes`; family-keyed `backend_enter_argv` + rm loops; `parse` via config enum; table test

## 4. Cleanup

- [x] 4.1 Delete `thegn-svc/src/ssh.rs`; theme reverse test; async-trait ratchet reseeded

## 5. Gate

- [x] 5.1 clippy core/svc/host; suites; lint; openspec validate; fmt
