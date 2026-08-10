# Tasks

## 1. Config surface

- [x] 1.1 Add `oci_runtime: String` to `SandboxConfig` + `Default` (config.rs)
- [x] 1.2 Document `oci_runtime` in `config/config.toml.example` under `[sandbox]`
- [x] 1.3 `config_example` invariant test stays green (key documented + parses)

## 2. Spec + compose (Phase 1: runsc)

- [x] 2.1 Add `oci_runtime: Option<String>` to `SandboxSpec`; set in the resolver
- [x] 2.2 Inject `--runtime <x>` in `oci_create_opts` for OCI backends when set
- [x] 2.3 Unit test: flag present only for OCI backends when set; blank ⇒ absent

## 3. Honest isolation class

- [x] 3.1 Thread `oci_runtime` through `Capabilities::{derive,from_parts}`
- [x] 3.2 `isolation_for`: `runsc → UserspaceKernel`, `krun → GuestKernel` (OCI only)
- [x] 3.3 Unit tests: mapping + egress stays Enforce + projection stays Bind

## 4. Detection + doctor

- [x] 4.1 Pure `sandbox_runtime::{runtime_req, decide}` (requirements + degrade rule) + tests
- [x] 4.2 Host probe + degrade before create (`remote_sync::finalize_spec_before_ensure`)
- [x] 4.3 `thegn doctor` reports the configured runtime + availability + raised class

## 5. Phase 2: krun (guest kernel)

- [x] 5.1 `krun → GuestKernel` mapping (covered by 3.2) + `/dev/kvm` requirement
- [ ] 5.2 Live-verify a `krun` pane on a `/dev/kvm` host (distinct guest `uname -r`)
- [ ] 5.3 (Secondary) map a local guest-kernel sandbox to `T3GuestKernel` if it
      ever enters multi-host packing

## 6. Validate

- [ ] 6.1 `just quick thegn-core` + `just quick thegn-host`
- [ ] 6.2 `just ci` (fmt + lint + build + test + openspec-validate + coverage + smoke)
