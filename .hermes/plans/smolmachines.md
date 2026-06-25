# Smolmachines Sandbox Provider Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Add full support for `smolmachines.com` (`smolvm`) as a sandbox provider to the Superzej IDE, enabling sub-second microVM isolation natively without Docker or Podman overhead.

**Architecture:** We will integrate `smolvm` as a first-class OCI-compatible sandbox backend within the existing fallback chain. The `SandboxBackend` enum will gain a `Smol` variant, mapped to `smolvm` runtime binary. Container lifecycle (`ensure`, `enter_argv`, `teardown`, etc.) will generate `smolvm` CLI commands (`machine create`, `machine exec`, `machine stop`, `machine delete`). The default network (`--net`) and volume binding semantics map cleanly.

**Tech Stack:** Rust (`crates/superzej-core`), TOML config (`crates/superzej-core/src/config.rs`), OCI execution semantics.

---

### Task 1: Extend Configuration `SandboxBackend`

**Objective:** Add `Smol` to the config-facing `SandboxBackend` enum with its string aliases.

**Files:**
- Modify: `crates/superzej-core/src/config.rs`

**Step 1: Write failing test**
Update the `config_enum_every_variant_roundtrips_canon_and_aliases` test to include:
```rust
("smol", SandboxBackend::Smol),
("smolvm", SandboxBackend::Smol),
```
Run `cargo test -p superzej-core --lib config::tests`

**Step 2: Minimal Implementation**
Add `Smol` to the `SandboxBackend` enum:
```rust
pub enum SandboxBackend: "sandbox backend" {
    // ...
    Apple = "apple" | "container",
    Wsl = "wsl",
    Smol = "smol" | "smolvm",
    None = "none" | "host",
}
```
Update `as_str()` assertions if needed.

**Step 3: Verify Pass**
Run tests, ensure PASS.

**Step 4: Commit**
`git add crates/superzej-core/src/config.rs`
`git commit -m "feat(sandbox): add Smol to SandboxBackend enum"`

---

### Task 2: Implement Runtime `Backend::Smol`

**Objective:** Add `Smol` to the runtime `Backend` enum and implement its mapping logic.

**Files:**
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 1: Write failing test**
Create/Update backend string parsing tests if they exist, or verify compiler errors catch the non-exhaustive match in `Backend` methods.

**Step 2: Minimal Implementation**
In `crates/superzej-core/src/sandbox.rs`:
Add variant:
```rust
pub enum Backend {
    // ...
    Wsl,
    Smol,
    None,
}
```
Add to `parse`:
```rust
"smol" | "smolvm" => Backend::Smol,
```
Add to `from_config`:
```rust
SandboxBackend::Smol => Backend::Smol,
```
Add to `label()`:
```rust
Backend::Smol => "smolvm",
```
Add to `binary()`:
```rust
Backend::Smol => "smolvm",
```
Add to `is_oci()`: (smol uses images and persistent named machines, so treat as OCI)
```rust
| Backend::Smol
```

**Step 3: Verify Pass**
Run `cargo check -p superzej-core` and fix any other `match backend { ... }` exhaustiveness errors in `sandbox.rs` (there will be several, e.g. `stats`, `available`, `teardown`). For now, just add placeholder `Backend::Smol => { ... }` or group with Docker/Podman if applicable. For `available`, check `util::have("smolvm")`. For `stats`, add a minimal struct or return empty stats.

**Step 4: Commit**
`git commit -a -m "feat(sandbox): add Backend::Smol runtime variant"`

---

### Task 3: Implement Lifecycle Hooks for Smol (Create / Ensure)

**Objective:** Implement `machine create` command generation for `smolvm` in the sandbox module.

**Files:**
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 1: Design CLI Map**
`smolvm` uses `smolvm machine create --name <container_name> --image <image> [--net] [-v host:guest]`
And `smolvm machine start --name <container_name>`

**Step 2: Update `ensure` function**
Find where `podman container create` is generated. Add a branch for `Backend::Smol`.
Implement a new `smol_create_opts(spec: &SandboxSpec) -> Vec<String>` helper to translate the spec to `smolvm machine create` args.
- `--net` if spec implies network
- `--image` `spec.image`
- `-v` `spec.mounts`
- `-e` `spec.env_passthrough`
- `--cpus`, `--memory` if in `spec.limits`
- `--gpu` if `spec.gpu` is present

Also update the health probe/existence check. `smolvm machine status --name <name>` returns status. Note that smolvm requires `machine start` after `machine create`. Make sure `ensure` calls `smolvm machine start` to ensure the VM is running.

**Step 3: Verify**
Add a unit test `test_smolvm_create_opts` mirroring `oci_create_opts_map_userns_and_mounts`.

**Step 4: Commit**
`git commit -a -m "feat(sandbox): implement smolvm ensure and create opts"`

---

### Task 4: Implement Lifecycle Hooks for Smol (Exec / Teardown)

**Objective:** Implement execution inside the VM and teardown.

**Files:**
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 1: Enter ARGV**
Update `enter_argv`:
```rust
        Backend::Smol => {
            let mut v = backend_prefix(Backend::Smol);
            v.extend([
                "machine".into(),
                "exec".into(),
                "--name".into(),
                spec.name.clone(),
                "--".into(),
            ]);
            // append cmd
            v.extend(cmd);
            v
        }
```

**Step 2: Teardown**
Update `teardown`:
```rust
        Backend::Smol => {
            let mut v = backend_prefix(Backend::Smol);
            v.extend(["machine".into(), "delete".into(), "--name".into(), name.to_string(), "-f".into()]);
            v
        }
```

**Step 3: Verify & Commit**
Run all tests.
`git commit -a -m "feat(sandbox): implement smolvm exec and teardown"`

---

### Task 5: Integration & Default Chain

**Objective:** Add `smolvm` to the default auto-detection chain.

**Files:**
- Modify: `crates/superzej-core/src/config.rs`

**Step 1: Update Default Chain**
Update `impl Default for SandboxConfig`:
```rust
            backend_chain: vec![
                "smolvm".into(),
                "podman-rootless".into(),
                "podman-rootful".into(),
                "docker".into(),
                "bwrap".into(),
                "none".into(),
            ],
```

**Step 2: Verify Tests**
Update `sandbox_config_default_collections` test in `config.rs` to assert the new chain. Add `smolvm` to the expected vector.

**Step 3: Commit**
`git commit -a -m "feat(sandbox): insert smolvm into default backend fallback chain"`
