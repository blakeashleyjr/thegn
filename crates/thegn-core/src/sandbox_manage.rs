//! Container-**management** op surface: the pure argv builders, output parsers,
//! and ownership witnesses behind the monitor's Containers tab, the `thegn
//! sandbox gc`/`prune` verbs, and the `containers.*` catalog rows.
//!
//! Three properties this module holds so the rest of the tree doesn't have to:
//!
//! 1. **Vendor dialects stay here.** docker/podman/apple-`container` strings
//!    appear only in these builders and in `sandbox.rs`'s backend profile
//!    table — never at a call site. A backend advertises the ops it supports
//!    ([`manage_ops`]); an unsupported op is *absent* (`None`), never a failing
//!    call. That is the sandbox spec's "backends are one profile table" rule
//!    extended to management.
//!
//! 2. **Ownership is STRUCTURAL, not reviewed-for.** A destructive argv can be
//!    built only for a resource wrapped in an [`OwnedContainer`] / [`OwnedImage`]
//!    / [`OwnedVolume`] witness, and every witness constructor rejects anything
//!    thegn does not own (the `thegn-` container-name family, the
//!    [`MANAGED_IMAGE_REPO`](crate::image::MANAGED_IMAGE_REPO) image-tag prefix,
//!    the `thegn.managed` volume label). There is no code path — not even a
//!    buggy one — that reaches a control/remove builder with a foreign name,
//!    because the type doesn't exist. "Never touch a foreign container" is a
//!    property of the types, so it can't be forgotten and can't be tested to
//!    bypass.
//!
//! 3. **Everything here is pure** (argv `Vec<String>` in, parsed structs out) so
//!    it lives in the 95%-covered core; the subprocess execution is in the host.
//!
//! Foreign containers are still *listed* (read-only, for context) — the
//! read-only [`parse_ps`] path keeps them — they are simply never wrapped in a
//! witness, so no action can target them.

use crate::sandbox::{Backend, CONTAINER_PREFIX};

/// The label every thegn-created **volume** (and the seed helper container)
/// carries, in `key=value` form. Images use [`crate::image::MANAGED_IMAGE_REPO`]
/// instead (a streamed base image gets no label); containers use the
/// [`CONTAINER_PREFIX`] name family.
pub const OWNED_LABEL: &str = "thegn.managed=true";

/// The `--filter` argument that scopes an engine listing to thegn-owned,
/// labelled resources. Hard-wired into every volume-listing builder so an
/// ownership-blind volume enumeration cannot be constructed.
pub const OWNED_LABEL_FILTER: &str = "label=thegn.managed=true";

/// The volume label whose value names the seeded warm-volume role
/// (`thegn-nix-store`, `thegn-cargo`, …). A role-labelled volume is a
/// deliberately-persistent dedup cache / user-state store — [`prune`] skips it.
pub const VOLUME_ROLE_LABEL: &str = "thegn.volume.role";

// --- ownership predicates ---------------------------------------------------

/// Is `name` a container thegn created? Every thegn container name — the
/// per-worktree `thegn-<slug>`, its profile variant, and the `-tgagent`/`-tgvpn`
/// companions — starts with [`CONTAINER_PREFIX`], so this one prefix test covers
/// the whole owned family (the same rule `ContainerInfo.ours` and
/// `identify_orphans` already use).
pub fn is_owned_container(name: &str) -> bool {
    name.starts_with(CONTAINER_PREFIX)
}

/// Is `reference` (a `repo:tag` image reference) a thegn base image? Owned
/// images live under the [`MANAGED_IMAGE_REPO`](crate::image::MANAGED_IMAGE_REPO)
/// repository; a foreign `docker.io/...` image never matches.
pub fn is_owned_image(reference: &str) -> bool {
    let repo = crate::image::MANAGED_IMAGE_REPO;
    reference == repo || reference.starts_with(&format!("{repo}:"))
}

/// Does a volume role value mark deliberately-persistent state that prune must
/// keep? Every volume thegn seeds today carries a role label (the nix-store /
/// cargo warm volumes, seeded once per host and mounted into every sandbox), so
/// a role-labelled managed volume is persistent by construction; a managed
/// volume with no role label is ephemeral and prunable. An unknown non-empty
/// role is treated as persistent — keeping cruft is cheaper than deleting user
/// state.
pub fn role_is_persistent(role: &str) -> bool {
    !role.trim().is_empty()
}

// --- ownership witnesses ----------------------------------------------------

/// Proof that a container name is thegn-owned. The ONLY constructor is
/// [`OwnedContainer::claim`], which returns `None` for a foreign name, so a
/// control/logs/exec argv literally cannot be built against a container thegn
/// did not create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedContainer(String);

impl OwnedContainer {
    /// Claim ownership of `name`, or `None` if it is not a thegn container.
    pub fn claim(name: &str) -> Option<OwnedContainer> {
        is_owned_container(name).then(|| OwnedContainer(name.to_string()))
    }

    pub fn name(&self) -> &str {
        &self.0
    }
}

/// A thegn-owned image, minted only by [`parse_owned_images`] (which applies the
/// [`is_owned_image`] filter), so [`mgmt_image_rm_argv`] can target only images
/// thegn delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedImage {
    /// The id or reference used for `image rm` (private: the witness is the
    /// only thing a remove builder accepts).
    remove_ref: String,
    /// The `repo:tag` reference, for the dry-run listing.
    pub reference: String,
    /// On-disk size in bytes, when the engine reported it.
    pub size_bytes: Option<u64>,
}

impl OwnedImage {
    pub fn remove_ref(&self) -> &str {
        &self.remove_ref
    }
}

/// A thegn-owned volume, minted only by [`parse_owned_volumes`] (fed an
/// `--filter label=thegn.managed=true` listing), so [`mgmt_volume_rm_argv`] can
/// target only labelled volumes. Carries the role so prune can skip + name the
/// persistent ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedVolume {
    name: String,
    pub role: String,
    pub size_bytes: Option<u64>,
}

impl OwnedVolume {
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Whether prune must keep this volume (a seeded warm volume / user state).
    pub fn is_persistent(&self) -> bool {
        role_is_persistent(&self.role)
    }
}

// --- per-backend op support (caps ⇔ optional ops) ---------------------------

/// Which management ops a backend advertises. Derived purely from the argv
/// builders — `manage_ops(b).df == mgmt_df_argv(b).is_some()` — so the doctor
/// report and the "degrade when a backend lacks df" path read one truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ManageOps {
    /// `ps -a` listing (running + stopped).
    pub list: bool,
    /// `stats --no-stream` per-container CPU/mem/net.
    pub stats: bool,
    /// `system df` aggregate disk usage.
    pub df: bool,
    /// `logs --tail`.
    pub logs: bool,
    /// stop / start / restart / rm.
    pub control: bool,
    /// image + volume prune (owned-only).
    pub prune: bool,
}

impl ManageOps {
    /// The supported op names, for the doctor report (empty = none).
    pub fn names(self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.list {
            v.push("list");
        }
        if self.stats {
            v.push("stats");
        }
        if self.df {
            v.push("df");
        }
        if self.logs {
            v.push("logs");
        }
        if self.control {
            v.push("control");
        }
        if self.prune {
            v.push("prune");
        }
        v
    }
}

/// docker-CLI-compatible daemon backends: the ones whose management verbs
/// (`stats`, `system df`, `logs`, `stop`/`start`/`restart`/`rm`, `image`/`volume
/// prune`) are verified against a real runtime. Podman (both), Docker. `smol`
/// and `wsl` are unverified ([`Backend::verified`] is `false`); `apple`'s CLI is
/// a different dialect (no `stats` templates, no `system df`). Those get at most
/// the read-only listing, never a guessed destructive verb — the same rule that
/// keeps the GC honest.
fn docker_cli_managed(b: Backend) -> bool {
    matches!(
        b,
        Backend::Podman | Backend::PodmanRootful | Backend::Docker
    )
}

/// Every backend that can be *listed* for the Containers tab / prune discovery:
/// the docker-CLI ones plus `smol` (docker clone, already swept by the GC) and
/// `apple` (`container ls`). Read-only, so unverified backends are safe here.
fn listable(b: Backend) -> bool {
    docker_cli_managed(b) || matches!(b, Backend::Smol | Backend::Apple)
}

/// The management ops `backend` supports.
pub fn manage_ops(backend: Backend) -> ManageOps {
    let d = docker_cli_managed(backend);
    ManageOps {
        list: listable(backend),
        stats: d,
        df: d,
        logs: d,
        control: d,
        prune: d,
    }
}

// --- read-only listing / stats / df argv ------------------------------------

/// `ps -a` (running + stopped) as JSON, for the Containers tab and prune
/// candidate discovery. `None` where the backend cannot be listed.
pub fn mgmt_list_argv(backend: Backend) -> Option<Vec<String>> {
    if !listable(backend) {
        return None;
    }
    Some(match backend {
        // Apple's `container` has no `ps` and no Go templates; `ls -a --format
        // json` carries the name in a top-level `id` (see `parse_container_list`).
        Backend::Apple => strs(&["ls", "-a", "--format", "json"]),
        // podman emits a JSON array; docker/smol emit NDJSON via a Go template.
        Backend::Podman | Backend::PodmanRootful => strs(&["ps", "-a", "--format", "json"]),
        _ => strs(&["ps", "-a", "--format", "{{json .}}"]),
    })
}

/// `stats --no-stream` per-container CPU/mem/net. `None` unless the backend has
/// a verified `stats` verb — the expensive op the visibility gate protects.
pub fn mgmt_stats_argv(backend: Backend) -> Option<Vec<String>> {
    docker_cli_managed(backend).then(|| strs(&["stats", "--no-stream", "--format", "json"]))
}

/// `system df` aggregate disk usage. `None` for apple (no `system df`) and the
/// unverified backends — the footprint header marks the total partial for those.
pub fn mgmt_df_argv(backend: Backend) -> Option<Vec<String>> {
    docker_cli_managed(backend).then(|| strs(&["system", "df", "--format", "json"]))
}

/// `logs --tail <n> <name>` for an owned container. Takes an [`OwnedContainer`],
/// so a foreign name cannot reach it.
pub fn mgmt_logs_argv(backend: Backend, c: &OwnedContainer, tail: u32) -> Option<Vec<String>> {
    docker_cli_managed(backend).then(|| {
        strs(&["logs", "--tail"])
            .into_iter()
            .chain([tail.to_string(), c.name().to_string()])
            .collect()
    })
}

// --- destructive / control argv (ownership-gated by type) -------------------

/// A lifecycle op on a single owned container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOp {
    Stop,
    Start,
    Restart,
    /// `rm -f` (force-remove, even if running).
    Remove,
}

impl ControlOp {
    fn verb(self) -> &'static str {
        match self {
            ControlOp::Stop => "stop",
            ControlOp::Start => "start",
            ControlOp::Restart => "restart",
            ControlOp::Remove => "rm",
        }
    }

    /// User-facing label (status/toast text, confirm prompts).
    pub fn label(self) -> &'static str {
        match self {
            ControlOp::Stop => "stop",
            ControlOp::Start => "start",
            ControlOp::Restart => "restart",
            ControlOp::Remove => "remove",
        }
    }
}

/// `stop|start|restart|rm <owned-container>`. The container argument is an
/// [`OwnedContainer`] witness, so the ownership check is the type system, not a
/// runtime `if` — there is no way to spell this call for a foreign container.
pub fn mgmt_control_argv(
    backend: Backend,
    op: ControlOp,
    c: &OwnedContainer,
) -> Option<Vec<String>> {
    if !docker_cli_managed(backend) {
        return None;
    }
    let mut v = vec![op.verb().to_string()];
    if op == ControlOp::Remove {
        // Force-remove so a still-running container is torn down in one step
        // (the tab double-confirms a running remove before we get here).
        v.push("-f".into());
    }
    v.push(c.name().to_string());
    Some(v)
}

/// `exec -it <owned-container> <cmd...>` for shell-in. Owned by type.
pub fn mgmt_exec_argv(backend: Backend, c: &OwnedContainer, cmd: &[&str]) -> Option<Vec<String>> {
    docker_cli_managed(backend).then(|| {
        strs(&["exec", "-it"])
            .into_iter()
            .chain([c.name().to_string()])
            .chain(cmd.iter().map(|s| s.to_string()))
            .collect()
    })
}

// --- owned image / volume listing + removal ---------------------------------

/// List images for prune discovery (`images --format json`). Not itself
/// ownership-filtered — the engine has no image-repo `--filter` — but the only
/// consumer is [`parse_owned_images`], which keeps only
/// [`is_owned_image`] references, and its `OwnedImage` output is the only thing
/// [`mgmt_image_rm_argv`] accepts.
pub fn mgmt_image_list_argv(backend: Backend) -> Option<Vec<String>> {
    if !docker_cli_managed(backend) {
        return None;
    }
    Some(match backend {
        Backend::Podman | Backend::PodmanRootful => strs(&["images", "--format", "json"]),
        _ => strs(&["images", "--format", "{{json .}}"]),
    })
}

/// `image rm <owned-image>`. Owned by type.
pub fn mgmt_image_rm_argv(backend: Backend, img: &OwnedImage) -> Option<Vec<String>> {
    docker_cli_managed(backend).then(|| {
        strs(&["image", "rm"])
            .into_iter()
            .chain([img.remove_ref().to_string()])
            .collect()
    })
}

/// List thegn-owned volumes: `volume ls --filter label=thegn.managed=true`. The
/// ownership filter is hard-wired — there is no unfiltered variant — so a
/// volume enumeration can only ever see thegn's own volumes.
pub fn mgmt_volume_list_argv(backend: Backend) -> Option<Vec<String>> {
    if !docker_cli_managed(backend) {
        return None;
    }
    let fmt = match backend {
        Backend::Podman | Backend::PodmanRootful => "json",
        _ => "{{json .}}",
    };
    Some(strs(&[
        "volume",
        "ls",
        "--filter",
        OWNED_LABEL_FILTER,
        "--format",
        fmt,
    ]))
}

/// `volume rm <owned-volume>`. Owned by type; the caller (prune) skips
/// persistent-role volumes before ever building this.
pub fn mgmt_volume_rm_argv(backend: Backend, vol: &OwnedVolume) -> Option<Vec<String>> {
    docker_cli_managed(backend).then(|| {
        strs(&["volume", "rm"])
            .into_iter()
            .chain([vol.name().to_string()])
            .collect()
    })
}

// --- parsers ----------------------------------------------------------------

/// Health rolled up from a container status string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Unhealthy,
    Starting,
    /// Running, no healthcheck.
    None,
    /// Not running.
    Stopped,
}

impl Health {
    pub fn label(self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::Unhealthy => "unhealthy",
            Health::Starting => "starting",
            Health::None => "up",
            Health::Stopped => "stopped",
        }
    }
}

/// Whether a `ps` status string denotes a running container. docker/podman say
/// `Up 3 minutes`; a stopped one says `Exited (0) …` / `Created`. Apple says
/// `running` / `stopped`.
pub fn container_running(status: &str) -> bool {
    let s = status.trim();
    s.starts_with("Up") || s.eq_ignore_ascii_case("running")
}

/// Roll a `ps` status string up to a [`Health`].
pub fn container_health(status: &str) -> Health {
    if !container_running(status) {
        return Health::Stopped;
    }
    let s = status.to_ascii_lowercase();
    if s.contains("unhealthy") {
        Health::Unhealthy
    } else if s.contains("health: starting") || s.contains("starting") {
        Health::Starting
    } else if s.contains("healthy") {
        Health::Healthy
    } else {
        Health::None
    }
}

/// Parse a human size string (`"1.2GB"`, `"512MB"`, `"0B"`, `"1.5kB"`, `"12MiB"`)
/// to bytes, or `None` if it isn't one. Handles SI (kB/MB/GB/TB) and binary
/// (KiB/MiB/GiB) suffixes; docker prints SI-ish, podman prints binary.
pub fn parse_size_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Split the numeric prefix from the unit suffix.
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.trim().parse().ok()?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((num * mult).round().max(0.0) as u64)
}

/// Format bytes as a compact human string (`"1.2 GB"`), SI, for the footprint
/// header. `0` renders `"0 B"`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Aggregate disk usage from a `system df` op, per resource kind: `(count,
/// bytes)`. Bytes are engine-wide totals (the footprint header labels them as
/// such); counts are shown alongside the owned counts the listings give.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskUsage {
    pub images: (u64, u64),
    pub containers: (u64, u64),
    pub volumes: (u64, u64),
}

/// Parse a `system df --format json` payload. podman emits a JSON array of
/// `{Type, Total, Size, ...}`; docker emits either one object or NDJSON of the
/// same per-type shape. Both are handled; unrecognised keys degrade to zero
/// rather than erroring (a partial number beats no number).
pub fn parse_system_df(output: &str) -> DiskUsage {
    let mut du = DiskUsage::default();
    let rows: Vec<serde_json::Value> =
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(output) {
            arr
        } else {
            output
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                .collect()
        };
    for r in rows {
        let Some(ty) = r.get("Type").and_then(|v| v.as_str()) else {
            continue;
        };
        let count = r
            .get("Total")
            .or_else(|| r.get("Count"))
            .or_else(|| r.get("TotalCount"))
            .and_then(json_u64)
            .unwrap_or(0);
        let bytes = df_size_bytes(&r);
        let ty = ty.to_ascii_lowercase();
        if ty.contains("image") {
            du.images = (count, bytes);
        } else if ty.contains("container") {
            du.containers = (count, bytes);
        } else if ty.contains("volume") {
            du.volumes = (count, bytes);
        }
    }
    du
}

/// The aggregate thegn container footprint for the Containers-tab header:
/// owned counts (precise, from ownership-filtered listings) plus the engine's
/// `df` byte totals. `partial` is set when a detected engine has no `df` op
/// (apple, unverified backends), so `disk` is a floor, not a total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContainerFootprint {
    /// Owned container count (thegn-name-prefixed), across detected backends.
    pub containers: u64,
    /// Owned image count (managed-repo tag).
    pub images: u64,
    /// Owned volume count (`thegn.managed` label).
    pub volumes: u64,
    /// Engine-wide disk usage from `df` (a superset of owned).
    pub disk: DiskUsage,
    /// A detected engine lacked the `df` op, so `disk` is partial.
    pub partial: bool,
}

impl ContainerFootprint {
    /// The engine-wide byte total across images, containers and volumes.
    pub fn total_bytes(&self) -> u64 {
        self.disk.images.1 + self.disk.containers.1 + self.disk.volumes.1
    }
}

fn df_size_bytes(r: &serde_json::Value) -> u64 {
    match r.get("Size") {
        // podman: integer bytes.
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        // docker: a human string ("1.2GB").
        Some(serde_json::Value::String(s)) => parse_size_bytes(s).unwrap_or(0),
        _ => 0,
    }
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Parse `images` output into the thegn-owned images only. Foreign images are
/// dropped here, so their references never reach a remove builder.
pub fn parse_owned_images(output: &str) -> Vec<OwnedImage> {
    rows_of(output)
        .into_iter()
        .filter_map(|r| {
            // podman: {Id, Names:[...], Size (int)}; docker: {ID, Repository,
            // Tag, Size ("123MB")}.
            let reference = image_reference(&r)?;
            if !is_owned_image(&reference) {
                return None;
            }
            let remove_ref = r
                .get("Id")
                .or_else(|| r.get("ID"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| reference.clone());
            let size_bytes = match r.get("Size") {
                Some(serde_json::Value::Number(n)) => n.as_u64(),
                Some(serde_json::Value::String(s)) => parse_size_bytes(s),
                _ => None,
            };
            Some(OwnedImage {
                remove_ref,
                reference,
                size_bytes,
            })
        })
        .collect()
}

fn image_reference(r: &serde_json::Value) -> Option<String> {
    // podman: `Names` is an array of full references.
    if let Some(name) = r
        .get("Names")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
    {
        return Some(name.to_string());
    }
    // docker: Repository + Tag.
    let repo = r.get("Repository").and_then(|v| v.as_str())?;
    let tag = r.get("Tag").and_then(|v| v.as_str()).unwrap_or("latest");
    Some(format!("{repo}:{tag}"))
}

/// Parse `volume ls --filter label=thegn.managed=true` output. Every row is
/// already owned by the filter; we parse the role for the persistent-skip.
pub fn parse_owned_volumes(output: &str) -> Vec<OwnedVolume> {
    rows_of(output)
        .into_iter()
        .filter_map(|r| {
            let name = r.get("Name").and_then(|v| v.as_str())?.to_string();
            let role = volume_role(&r);
            let size_bytes = match r.get("Size") {
                Some(serde_json::Value::Number(n)) => n.as_u64(),
                Some(serde_json::Value::String(s)) => parse_size_bytes(s),
                _ => None,
            };
            Some(OwnedVolume {
                name,
                role,
                size_bytes,
            })
        })
        .collect()
}

fn volume_role(r: &serde_json::Value) -> String {
    match r.get("Labels") {
        // podman: Labels is a map.
        Some(serde_json::Value::Object(m)) => m
            .get(VOLUME_ROLE_LABEL)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        // docker: Labels is a comma-joined `k=v` string.
        Some(serde_json::Value::String(s)) => s
            .split(',')
            .find_map(|kv| kv.trim().strip_prefix(&format!("{VOLUME_ROLE_LABEL}=")))
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Decode a JSON payload that is either a top-level array (podman) or NDJSON
/// (docker Go-template `{{json .}}`).
fn rows_of(output: &str) -> Vec<serde_json::Value> {
    if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(output) {
        return arr;
    }
    output
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .collect()
}

fn strs(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
#[path = "sandbox_manage_tests.rs"]
mod tests;
