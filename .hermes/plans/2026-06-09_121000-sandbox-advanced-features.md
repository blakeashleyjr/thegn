# Superzej Advanced Sandbox Capabilities Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Expand Superzej's sandbox architecture to support resource limits, GPU passthrough, image prefetching, named volumes, health checks, multi-container compose, and audit logging. Support declarative definitions via `.superzej.toml`.

**Architecture:**
Expand the `SandboxConfig` and `SandboxOverlay` structs to map to new OCI configuration parameters. Modify `sandbox::oci_create_opts` to translate these configurations into exact `podman`/`docker` CLI flags (like `--cpus`, `--memory`, `--device`, `-v volume:dest`). Add logic for new hooks (like health checks and prefetching). Provide a compose translation layer.

**Tech Stack:** Rust, serde, Podman/Docker CLI, sqlite.

---

### Task 1: Resource Limits & Quotas

**Objective:** Add `limits.cpu` and `limits.memory` to the `SandboxConfig` and apply them to OCI arguments.

**Files:**
- Modify: `crates/superzej-core/src/config.rs`
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 1: Write failing config parsing test**
```rust
// In crates/superzej-core/src/config.rs
#[test]
fn test_sandbox_limits_parse() {
    let toml = r#"
        [sandbox.limits]
        cpu = "2"
        memory = "4GB"
    "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.sandbox.limits.cpu, Some("2".to_string()));
    assert_eq!(cfg.sandbox.limits.memory, Some("4GB".to_string()));
}
```

**Step 2: Run test to verify failure**
Run: `cargo nextest run test_sandbox_limits_parse`
Expected: FAIL

**Step 3: Write minimal implementation**
```rust
// In crates/superzej-core/src/config.rs
#[derive(Debug, Clone, Default, serde::Deserialize, PartialEq)]
#[serde(default)]
pub struct SandboxLimits {
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

// Add `pub limits: SandboxLimits` to SandboxConfig and SandboxOverlay
// Update `impl Default for SandboxConfig` and `apply` logic.

// In crates/superzej-core/src/sandbox.rs
// Inside SandboxSpec add `pub limits: SandboxLimits`

// In `oci_create_opts`
if let Some(c) = &spec.limits.cpu {
    v.extend(["--cpus".into(), c.clone()]);
}
if let Some(m) = &spec.limits.memory {
    v.extend(["--memory".into(), m.clone()]);
}
```

**Step 4: Run test to verify pass**
Run: `cargo nextest run test_sandbox_limits_parse`
Expected: PASS

**Step 5: Commit**
```bash
git add crates/superzej-core/src/config.rs crates/superzej-core/src/sandbox.rs
git commit -m "feat(sandbox): support cpu and memory resource limits"
```

---

### Task 2: GPU Passthrough

**Objective:** Allow exposing GPU hardware to the container via `sandbox.gpu = "nvidia" | "intel" | "all"`.

**Files:**
- Modify: `crates/superzej-core/src/config.rs`
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 1: Write failing test**
```rust
// In crates/superzej-core/src/sandbox.rs
#[test]
fn test_oci_create_opts_gpu() {
    let mut spec = spec(Backend::Docker);
    spec.gpu = Some("all".into());
    let opts = oci_create_opts(&spec);
    assert!(opts.contains(&"--gpus".to_string()));
    assert!(opts.contains(&"all".to_string()));
}
```

**Step 3: Write minimal implementation**
```rust
// In config.rs add `pub gpu: Option<String>` to SandboxConfig and Overlay.
// In sandbox.rs SandboxSpec add `pub gpu: Option<String>`.

// In `oci_create_opts`:
if let Some(gpu) = &spec.gpu {
    if spec.backend == Backend::Docker {
        v.extend(["--gpus".into(), gpu.clone()]);
    } else if spec.backend == Backend::Podman {
        v.extend(["--device".into(), "nvidia.com/gpu=all".into()]);
    }
}
```

**Step 5: Commit**
```bash
git add crates/superzej-core/src/config.rs crates/superzej-core/src/sandbox.rs
git commit -m "feat(sandbox): add gpu passthrough support"
```

---

### Task 3: Named Volumes

**Objective:** Add `sandbox.volumes` dict mapping `name = "/container/path"` for persistent storage across worktree reboots.

**Files:**
- Modify: `crates/superzej-core/src/config.rs`
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 1: Write failing test**
```rust
// In sandbox.rs tests
#[test]
fn test_oci_create_opts_volumes() {
    let mut spec = spec(Backend::Podman);
    spec.volumes.push(("my-data".into(), "/var/data".into()));
    let opts = oci_create_opts(&spec);
    assert!(opts.join(" ").contains("-v my-data:/var/data"));
}
```

**Step 3: Write minimal implementation**
```rust
// Add `pub volumes: std::collections::HashMap<String, String>` to SandboxConfig
// Map it into `SandboxSpec::volumes: Vec<(String, String)>`.

// In `oci_create_opts`:
for (vol_name, dest) in &spec.volumes {
    v.extend(["-v".into(), format!("{}:{}", vol_name, dest)]);
}
```

**Step 5: Commit**
```bash
git add crates/superzej-core/src/config.rs crates/superzej-core/src/sandbox.rs
git commit -m "feat(sandbox): support declarative named volumes"
```

---

### Task 4: Image Prefetching

**Objective:** Add a routine to automatically `podman pull` images in the background if they don't exist.

**Files:**
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 1: Write failing test**
```rust
// mock or integration test omitted for brevity; unit test the argument builder
```

**Step 3: Write minimal implementation**
```rust
pub fn prefetch_image(spec: &SandboxSpec) -> Result<()> {
    if !spec.backend.is_oci() { return Ok(()); }
    if let Some(img) = &spec.image {
        let rt = spec.backend.binary();
        let _ = std::process::Command::new(rt)
            .args(["image", "exists", img])
            .output().map_err(|_| anyhow::anyhow!("backend not found"))?;
        // Simplistic check: if not 0, pull. 
        // Real implementation:
        std::process::Command::new(rt)
            .args(["pull", img])
            .output()?;
    }
    Ok(())
}
```

**Step 5: Commit**
```bash
git add crates/superzej-core/src/sandbox.rs
git commit -m "feat(sandbox): add image prefetching logic"
```

---

### Task 5: Health Checks

**Objective:** Verify the persistent keep-alive container is responsive before entering.

**Files:**
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 3: Write minimal implementation**
```rust
pub fn health_check(spec: &SandboxSpec) -> bool {
    if !spec.backend.is_oci() { return true; }
    let rt = spec.backend.binary();
    let out = std::process::Command::new(rt)
        .args(["exec", &spec.name, "echo", "ok"])
        .output().ok();
    out.map(|o| o.status.success()).unwrap_or(false)
}
// Add call into `pick_agent.rs` if `sandbox::ensure` returns true.
```

**Step 5: Commit**
```bash
git add crates/superzej-core/src/sandbox.rs
git commit -m "feat(sandbox): add pre-entry container health check"
```

---

### Task 6: Compose / Multi-Container Translation Layer

**Objective:** Add `sandbox.compose` boolean. If true, translate `.superzej.toml` into a dynamic `docker-compose.yml` or `podman-compose` run. 
*Note:* This is complex. The MVP will be a configuration flag that simply delegates to a `docker-compose up -d` script on `ensure()`.

**Files:**
- Modify: `crates/superzej-core/src/config.rs`
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 3: Write minimal implementation**
```rust
// Add `pub compose: Option<String>` (path to compose file) to config.
// In `ensure()`:
if let Some(compose_file) = &spec.compose {
    let _ = std::process::Command::new("docker-compose")
        .args(["-f", compose_file, "-p", &spec.name, "up", "-d"])
        .output()?;
}
```

**Step 5: Commit**
```bash
git add crates/superzej-core/src/config.rs crates/superzej-core/src/sandbox.rs
git commit -m "feat(sandbox): initial docker-compose delegation support"
```

---

### Task 7: Audit Logging

**Objective:** Append `exec` and `ensure` events to an `audit.log` in `$SUPERZEJ_DIR`.

**Files:**
- Modify: `crates/superzej-core/src/log.rs`
- Modify: `crates/superzej-core/src/sandbox.rs`

**Step 3: Write minimal implementation**
```rust
// In `sandbox.rs`, inject tracing calls. 
// "User {} spawned agent {} in worktree {} via {}"
```

**Step 5: Commit**
```bash
git commit -m "feat(sandbox): add sandbox audit logging events"
```