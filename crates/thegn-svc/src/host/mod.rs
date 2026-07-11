//! Host provisioning **runners**: the impure executors behind the pure
//! [`thegn_core::host_machine`] state machine. A runner turns each
//! `HostEffect` into real work over a control channel — local subprocess or
//! ssh (multiplexed `ControlMaster` via [`Placement::control_argv`]); iroh
//! lowers to ssh over a local forward before a runner ever sees it. All
//! methods are BLOCKING (subprocess + deadline, the `sandbox.rs` pattern) and
//! run inside `spawn_blocking`/CLI threads — never on the compositor loop.
//!
//! Timeouts follow the host-flow policy table: connect 30s, probe 60s,
//! install 900s, resolve 60s, deliver 1800s (+ stall rule in the streaming
//! strategies), volume seed 1800s.

use std::time::{Duration, Instant};

use thegn_core::host::{
    Arch, DeliveryCap, HostCaps, IrohReach, Reach, RuntimeInfo, RuntimeKind, VolumeSeed, VolumeSpec,
};
use thegn_core::image::{DeliveryStrategy, Digest, ImageRef, LocalCaps, ResolvedImage};
use thegn_core::placement::Placement;
use thegn_core::transport_error::{
    ClassifiedErr, ErrorClass, classify_exec, describe_exec_failure,
};

mod cloud;
mod deliver;
mod retry;

pub use cloud::cloud_runner_for;
pub use deliver::stream_archive_over_ssh;

/// Everything the host-flow driver needs to execute effects against one host.
/// Implementations own their per-step deadlines; errors are [`ClassifiedErr`]s
/// (transient network flap vs durable refusal) so the driver's retry ladder
/// knows what's worth another attempt before an error flows into
/// `HostEvent::*Failed` (and from there into actionable failures).
pub trait HostRunner: Send {
    /// Open (or re-verify) the control channel. For ssh this warms the
    /// ControlMaster so every later exec rides one TCP/auth handshake.
    fn connect(&mut self) -> Result<(), ClassifiedErr>;
    /// Run the single-shot probe and parse its `KEY=VALUE` contract.
    fn probe(&mut self) -> Result<HostCaps, ClassifiedErr>;
    /// Bootstrap a runtime. The driver only calls this with consent granted.
    fn install_runtime(
        &mut self,
        kind: RuntimeKind,
        note: &mut dyn FnMut(String),
    ) -> Result<RuntimeInfo, ClassifiedErr>;
    /// Resolve the image reference to its per-arch digests.
    fn resolve_image(&mut self, reference: &ImageRef) -> Result<ResolvedImage, ClassifiedErr>;
    /// Is `name@digest` already in the host's image storage?
    fn image_present(&mut self, image: &ImageRef, digest: &Digest) -> Result<bool, ClassifiedErr>;
    /// Execute one delivery strategy; returns the digest VERIFIED on the host.
    fn deliver(
        &mut self,
        strategy: DeliveryStrategy,
        image: &ImageRef,
        digest: &Digest,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Digest, ClassifiedErr>;
    /// Idempotently seed one warm volume (exists ⇒ no-op success).
    fn seed_volume(
        &mut self,
        spec: &VolumeSpec,
        image: &ImageRef,
        digest: &Digest,
    ) -> Result<(), ClassifiedErr>;
    /// The remote OCI daemon URL sandbox spawn should pin (`None` ⇒ the
    /// placement transport wraps the whole argv, as today).
    fn oci_url(&self) -> Option<String>;
    /// One cheap live-resources sample (the placement engine's measured
    /// layer). Default: unsupported — cloud runners have no shell to ask.
    fn probe_headroom(&mut self) -> Result<thegn_core::host_probe::Headroom, ClassifiedErr> {
        Err("headroom probe unsupported for this reach".into())
    }
}

/// Build the runner for a reach. `Reach::Iroh` LOWERS to ssh over a local
/// dumbpipe forward before the runner exists — everything above (probe,
/// install, delivery) is byte-identical to the ssh path. Cloud is lowered by
/// its own adapter (phase 6), never a subprocess runner.
pub fn runner_for(reach: &Reach) -> Result<Box<dyn HostRunner>, String> {
    match reach {
        Reach::Cloud(c) => cloud::cloud_runner_for(c),
        other => oci_runner_for(other).map(|r| Box::new(r) as Box<dyn HostRunner>),
    }
}

/// The concrete subprocess runner for local/ssh/iroh reaches — for callers
/// that need its inherent container-exec/tar-push methods (the per-worktree
/// applier). Cloud reaches have no subprocess runner.
pub fn oci_runner_for(reach: &Reach) -> Result<OciRunner, String> {
    match reach {
        Reach::Local => Ok(OciRunner::new(Placement::Local)),
        Reach::Ssh(p) => Ok(OciRunner::new(Placement::Ssh(p.clone()))),
        Reach::Iroh(i) => {
            let (tunnel, placement) = lower_iroh(i)?;
            let mut r = OciRunner::new(Placement::Ssh(placement));
            r.tunnel = Some(tunnel);
            Ok(r)
        }
        Reach::Cloud(c) => Err(format!(
            "cloud host ({}) has no subprocess runner",
            c.provider
        )),
    }
}

/// A live dumbpipe TCP forward to a NAT'd host's sshd. Held by the runner so
/// the tunnel outlives every exec of the provisioning run; killed on drop.
/// (Interactive PANES over iroh need a longer-lived tunnel manager — that's a
/// follow-up; host provisioning is self-contained here.)
pub struct IrohTunnel {
    child: std::process::Child,
}

impl Drop for IrohTunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `dumbpipe connect-tcp` on an ephemeral local port and lower the reach
/// to a plain ssh placement at `127.0.0.1:<port>`. `HostKeyAlias` keeps
/// known_hosts stable across ephemeral ports.
fn lower_iroh(i: &IrohReach) -> Result<(IrohTunnel, thegn_core::placement::SshPlacement), String> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let mut child = Command::new("dumbpipe")
        .args(["connect-tcp", "--addr", "127.0.0.1:0", i.ticket.trim()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("iroh: dumbpipe not runnable: {e}"))?;

    // dumbpipe prints its bound listen address; scrape the port off-thread
    // with a deadline (both streams — versions differ on where it lands).
    let (tx, rx) = std::sync::mpsc::channel::<u16>();
    let scan = |r: Box<dyn std::io::Read + Send>, tx: std::sync::mpsc::Sender<u16>| {
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(r);
            for line in reader.lines().map_while(Result::ok) {
                if let Some(idx) = line.find("127.0.0.1:") {
                    let tail = &line[idx + "127.0.0.1:".len()..];
                    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if let Ok(p) = digits.parse::<u16>()
                        && p != 0
                    {
                        let _ = tx.send(p);
                        return;
                    }
                }
            }
        });
    };
    if let Some(out) = child.stdout.take() {
        scan(Box::new(out), tx.clone());
    }
    if let Some(err) = child.stderr.take() {
        scan(Box::new(err), tx);
    }
    let port = rx.recv_timeout(Duration::from_secs(30)).map_err(|_| {
        let _ = child.kill();
        let _ = child.wait();
        "iroh: dumbpipe printed no local listen address within 30s \
             (bad ticket, or the remote listener is down)"
            .to_string()
    })?;
    let alias = format!("thegn-iroh-{}", thegn_core::util::short_hash(&i.ticket, 8));
    let placement = thegn_core::placement::SshPlacement {
        host: format!("{}@127.0.0.1", i.user),
        port,
        forward_agent: false,
        kind: thegn_core::placement::TransportKind::Ssh,
        ssh_config: None,
        jump_host: None,
        identity: None,
        extra_args: vec![
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            format!("HostKeyAlias={alias}"),
        ],
    };
    Ok((IrohTunnel { child }, placement))
}

/// Probe the LOCAL side's delivery abilities (cheap `command -v` checks).
pub fn local_caps() -> LocalCaps {
    let has = |bin: &str| {
        exec_argv(
            &["sh".into(), "-c".into(), format!("command -v {bin}")],
            Duration::from_secs(5),
        )
        .map(|o| o.ok)
        .unwrap_or(false)
    };
    LocalCaps {
        has_podman: has("podman"),
        has_skopeo: has("skopeo"),
        has_rsync: has("rsync"),
        has_registry_egress: true, // refined per-pull; assume the dev box has egress
    }
}

/// The single-shot probe script. Emits the `KEY=VALUE` contract parsed by
/// [`HostCaps::parse_probe`] — extend both together.
const PROBE_SCRIPT: &str = r#"
set -u
echo "ARCH=$(uname -m)"
echo "OS=$(uname -s | tr '[:upper:]' '[:lower:]')"
if command -v podman >/dev/null 2>&1; then
  echo "PODMAN=$(podman --version 2>/dev/null | awk '{print $NF}')"
  [ "$(id -u)" != "0" ] && echo "PODMAN_ROOTLESS=1"
  s="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/podman/podman.sock"
  [ -S "$s" ] && echo "PODMAN_SOCKET=$s"
fi
if command -v docker >/dev/null 2>&1; then
  echo "DOCKER=$(docker --version 2>/dev/null | sed 's/^Docker version //; s/,.*//')"
fi
for pm in apt dnf apk pacman; do
  if command -v "$pm" >/dev/null 2>&1; then echo "PKGMGR=$pm"; break; fi
done
echo "NPROC=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"
awk '/MemTotal/ {print "MEM_TOTAL_KB=" $2}' /proc/meminfo 2>/dev/null
[ -f /sys/fs/cgroup/cgroup.controllers ] && echo "CGROUPV2=1"
[ "$(cat /proc/sys/kernel/unprivileged_userns_clone 2>/dev/null || echo 1)" = "1" ] && echo "USERNS=1"
command -v skopeo >/dev/null 2>&1 && echo "SKOPEO=1"
command -v rsync  >/dev/null 2>&1 && echo "RSYNC=1"
command -v nix    >/dev/null 2>&1 && echo "NIX=1"
df -kP "${HOME:-/}" 2>/dev/null | awk 'NR==2 {print "DISK_FREE=" $4 * 1024}'
if command -v curl >/dev/null 2>&1; then
  curl -fsS --max-time 5 -o /dev/null https://ghcr.io/v2/ 2>/dev/null; rc=$?
  # 0 = reachable; 22 = HTTP error (reachable, auth-gated) — both mean egress.
  if [ "$rc" = "0" ] || [ "$rc" = "22" ]; then echo "EGRESS=full"; else echo "EGRESS=none"; fi
fi
true
"#;

/// The live-resources sample (the placement engine's measured layer): one
/// cheap exec, parsed by the pure `thegn_core::host_probe::parse_headroom`
/// (extend BOTH together — the contract test below pins agreement).
const HEADROOM_SCRIPT: &str = r#"
set -u
echo "NPROC=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"
awk '/MemTotal/ {print "MEM_TOTAL_KB=" $2} /MemAvailable/ {print "MEM_AVAIL_KB=" $2}' /proc/meminfo 2>/dev/null
awk '{printf "LOAD1_MILLI=%d
", $1 * 1000}' /proc/loadavg 2>/dev/null
df -kP "${HOME:-/}" 2>/dev/null | awk 'NR==2 {print "DISK_FREE=" $4 * 1024}'
if command -v podman >/dev/null 2>&1; then
  echo "CONTAINERS=$(podman ps -q 2>/dev/null | wc -l | tr -d ' ')"
elif command -v docker >/dev/null 2>&1; then
  echo "CONTAINERS=$(docker ps -q 2>/dev/null | wc -l | tr -d ' ')"
fi
true
"#;

/// A finished control-plane exec: the command ran to completion (successfully
/// or not) before the deadline. Carries the exit code so transport drops
/// (ssh exit 255) stay distinguishable from real remote-command failures.
#[derive(Debug, Clone)]
pub(crate) struct ExecOut {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOut {
    /// One-line failure message naming the real cause (exit code + stderr tail
    /// + transport classification) — never the old useless `"(no output)"`.
    pub(crate) fn msg(&self, label: &str) -> String {
        describe_exec_failure(label, self.code, false, &self.stderr)
    }

    /// Transient (network flap — retry) vs permanent (durable refusal).
    pub(crate) fn class(&self) -> ErrorClass {
        classify_exec(self.code, false, &self.stderr)
    }

    /// The message + classification as one [`ClassifiedErr`].
    pub(crate) fn cerr(&self, label: &str) -> ClassifiedErr {
        ClassifiedErr {
            class: self.class(),
            msg: self.msg(label),
        }
    }

    fn tuple(self) -> (bool, String, String) {
        (self.ok, self.stdout, self.stderr)
    }
}

/// The exec never completed: spawn error, deadline kill, or wait error.
#[derive(Debug, Clone)]
pub(crate) enum ExecFail {
    Spawn(String),
    Timeout { secs: u64 },
    Wait(String),
}

impl ExecFail {
    pub(crate) fn msg(&self, label: &str) -> String {
        match self {
            ExecFail::Spawn(e) => format!("{label}: spawn: {e}"),
            ExecFail::Timeout { secs } => {
                format!("{label}: timed out after {secs}s — slow or lossy link?")
            }
            ExecFail::Wait(e) => format!("{label}: wait: {e}"),
        }
    }

    pub(crate) fn class(&self) -> ErrorClass {
        match self {
            // A deadline kill on a flaky link is worth retrying; a spawn/wait
            // failure is local breakage no retry fixes.
            ExecFail::Timeout { .. } => ErrorClass::Transient,
            ExecFail::Spawn(_) | ExecFail::Wait(_) => ErrorClass::Permanent,
        }
    }

    /// The message + classification as one [`ClassifiedErr`].
    pub(crate) fn cerr(&self, label: &str) -> ClassifiedErr {
        ClassifiedErr {
            class: self.class(),
            msg: self.msg(label),
        }
    }
}

/// Run `argv` to completion with a hard deadline, capturing stdout + stderr
/// and the exit code. `Err` only when the exec never completed (spawn failure,
/// deadline kill, wait error) — a nonzero exit is an `Ok(ExecOut)` answer.
fn exec_argv(argv: &[String], timeout: Duration) -> Result<ExecOut, ExecFail> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ExecFail::Spawn(format!("{}: {e}", argv[0])))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                let mut err = String::new();
                if let Some(mut r) = child.stdout.take() {
                    let _ = r.read_to_string(&mut out);
                }
                if let Some(mut r) = child.stderr.take() {
                    let _ = r.read_to_string(&mut err);
                }
                return Ok(ExecOut {
                    ok: status.success(),
                    code: status.code(),
                    stdout: out,
                    stderr: err,
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExecFail::Timeout {
                    secs: timeout.as_secs(),
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(ExecFail::Wait(e.to_string())),
        }
    }
}

/// The `containers-storage:[driver@graphroot+runroot]` prefix for the LOCAL
/// rootless podman store, so skopeo reads a `just image-build`-loaded image (the
/// bare `containers-storage:` transport hits the *root* store — "mkdir
/// /run/containers: permission denied" — when run rootless). `None` if local
/// podman isn't available (callers then fall back to docker-daemon / registry).
pub(super) fn local_containers_storage_prefix() -> Option<String> {
    let out = exec_argv(
        &[
            "podman".into(),
            "info".into(),
            "--format".into(),
            "{{.Store.GraphDriverName}}|{{.Store.GraphRoot}}|{{.Store.RunRoot}}".into(),
        ],
        Duration::from_secs(10),
    )
    .ok()?;
    if !out.ok {
        return None;
    }
    let out = out.stdout;
    let mut parts = out.lines().next()?.trim().split('|');
    let drv = parts.next()?.trim();
    let gr = parts.next()?.trim();
    let rr = parts.next()?.trim();
    if drv.is_empty() || gr.is_empty() {
        return None;
    }
    Some(format!("containers-storage:[{drv}@{gr}+{rr}]"))
}

/// The subprocess-backed runner for local and ssh reaches. Every remote
/// command goes through `placement.control_argv(["sh","-lc",cmd])` — one
/// multiplexed ControlMaster connection for the whole provision.
pub struct OciRunner {
    placement: Placement,
    caps: Option<HostCaps>,
    /// A live iroh forward this runner's ssh placement rides (kept alive for
    /// the whole provisioning run; killed when the runner drops).
    tunnel: Option<IrohTunnel>,
}

impl OciRunner {
    pub fn new(placement: Placement) -> OciRunner {
        OciRunner {
            placement,
            caps: None,
            tunnel: None,
        }
    }

    fn is_local(&self) -> bool {
        matches!(self.placement, Placement::Local)
    }

    /// Run a shell command on the host with a deadline. On transport-failure
    /// evidence (exit 255 / deadline kill) the ControlMaster is health-checked
    /// and a dead socket cleared, so the caller's *next* attempt builds a
    /// fresh connection instead of failing identically against a wedged master.
    fn exec(&self, cmd: &str, timeout: Duration) -> Result<ExecOut, ExecFail> {
        let r = exec_argv(&self.control_shell_argv(cmd), timeout);
        match &r {
            Ok(o) if o.code == Some(255) => retry::master_hygiene(&self.placement),
            Err(ExecFail::Timeout { .. }) => retry::master_hygiene(&self.placement),
            _ => {}
        }
        r
    }

    /// The full local argv that runs `cmd` in a shell ON the host (identity
    /// for local, control-plane ssh wrap for remote) — the seam the streaming
    /// delivery uses to pipe stdin through.
    pub(crate) fn control_shell_argv(&self, cmd: &str) -> Vec<String> {
        self.placement
            .control_argv(&["sh".to_string(), "-lc".to_string(), cmd.to_string()])
    }

    pub(crate) fn placement(&self) -> &Placement {
        &self.placement
    }

    /// Run a shell command ON the host (not inside a container) with a deadline,
    /// returning `(success, stdout, stderr)`. Needs no probe — it only uses the
    /// placement's control transport (ssh with the configured ProxyCommand etc.).
    /// Used by the remote-worktree materializer (mkdir / git clone on the host).
    pub fn host_exec(
        &self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<(bool, String, String), String> {
        self.exec(cmd, timeout)
            .map(ExecOut::tuple)
            .map_err(|f| f.msg("host exec"))
    }

    /// Run a shell command INSIDE a container on this host (the per-worktree
    /// provisioning applier's exec primitive). The runtime binary comes from
    /// the probe; call after [`HostRunner::probe`].
    pub fn exec_in_container(
        &self,
        container: &str,
        cmd: &str,
        timeout: Duration,
    ) -> Result<(bool, String, String), String> {
        let bin = self.runtime_bin()?;
        self.exec(
            &format!(
                "{bin} exec {container} /bin/sh -lc {}",
                thegn_core::util::sh_quote(cmd)
            ),
            timeout,
        )
        .map(ExecOut::tuple)
        .map_err(|f| f.msg("container exec"))
    }

    /// Pipe a locally-produced stream into a command on the host: spawn
    /// `local_argv` (stdout piped) and the host command (stdin piped) and pump
    /// one into the other. The substrate for tar-over-exec file delivery
    /// (dotfiles / agent configs into a container on a remote host).
    pub fn pipe_local_to_host(
        &self,
        local_argv: &[String],
        host_cmd: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        use std::process::{Command, Stdio};
        let mut producer = Command::new(&local_argv[0])
            .args(&local_argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("pipe: spawn {}: {e}", local_argv[0]))?;
        let out = producer
            .stdout
            .take()
            .ok_or("pipe: producer has no stdout")?;
        let argv = self.control_shell_argv(host_cmd);
        let mut consumer = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::from(out))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("pipe: spawn host cmd: {e}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            match consumer.try_wait() {
                Ok(Some(status)) => {
                    let _ = producer.wait();
                    if status.success() {
                        return Ok(());
                    }
                    let mut err = String::new();
                    if let Some(mut r) = consumer.stderr.take() {
                        use std::io::Read;
                        let _ = r.read_to_string(&mut err);
                    }
                    return Err(describe_exec_failure(
                        "pipe: host cmd",
                        status.code(),
                        false,
                        &err,
                    ));
                }
                Ok(None) if Instant::now() >= deadline => {
                    let _ = consumer.kill();
                    let _ = producer.kill();
                    let _ = consumer.wait();
                    let _ = producer.wait();
                    return Err(format!("pipe: timed out after {}s", timeout.as_secs()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(e) => return Err(format!("pipe: wait: {e}")),
            }
        }
    }

    /// Tar a local staging directory into `dest` inside a container on this
    /// host (creates `dest` first; ownership follows the container user).
    pub fn push_dir_to_container(
        &self,
        container: &str,
        staging: &std::path::Path,
        dest: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let bin = self.runtime_bin()?;
        let local = vec![
            "tar".to_string(),
            "-C".to_string(),
            staging.to_string_lossy().into_owned(),
            "-cf".to_string(),
            "-".to_string(),
            ".".to_string(),
        ];
        let host_cmd = format!(
            "{bin} exec -i {container} /bin/sh -lc {}",
            thegn_core::util::sh_quote(&format!("mkdir -p {dest} && tar -xf - -C {dest}"))
        );
        self.pipe_local_to_host(&local, &host_cmd, timeout)
    }

    /// Run a shell command LOCALLY with a deadline.
    fn exec_local(cmd: &str, timeout: Duration) -> Result<ExecOut, ExecFail> {
        exec_argv(
            &["sh".to_string(), "-lc".to_string(), cmd.to_string()],
            timeout,
        )
    }

    fn runtime(&self) -> Result<&RuntimeInfo, String> {
        self.caps
            .as_ref()
            .and_then(|c| c.runtime.as_ref())
            .ok_or_else(|| "runner: no probed runtime (driver bug)".to_string())
    }

    fn runtime_bin(&self) -> Result<&'static str, String> {
        Ok(match self.runtime()?.kind {
            RuntimeKind::Podman => "podman",
            RuntimeKind::Docker => "docker",
            RuntimeKind::CloudManaged => {
                return Err("cloud-managed runtime has no host binary".into());
            }
        })
    }

    /// The skopeo storage transport that writes into THIS runtime's image
    /// store: podman reads `containers-storage:`, docker its daemon via
    /// `docker-daemon:`. (podman's store is invisible to `docker run`, and
    /// vice-versa — picking the wrong one loads an image the spawn can't see.)
    fn storage_transport(&self) -> Result<&'static str, String> {
        Ok(match self.runtime()?.kind {
            RuntimeKind::Podman => "containers-storage",
            RuntimeKind::Docker => "docker-daemon",
            RuntimeKind::CloudManaged => {
                return Err("cloud-managed runtime has no host binary".into());
            }
        })
    }

    /// A shell test that succeeds iff the named `image`/`volume` exists,
    /// spelled for the host's runtime. podman has the `… exists` sugar (exit
    /// 0/1); docker has no such subcommand, so use `… inspect >/dev/null`.
    fn exists_test(&self, kind: &str, name: &str) -> Result<String, String> {
        let bin = self.runtime_bin()?;
        Ok(match self.runtime()?.kind {
            RuntimeKind::Podman => format!("{bin} {kind} exists {name}"),
            RuntimeKind::Docker => format!("{bin} {kind} inspect {name} >/dev/null 2>&1"),
            RuntimeKind::CloudManaged => {
                return Err("cloud-managed runtime has no host binary".into());
            }
        })
    }

    /// Shell command that loads the verified oci-archive at `archive` into the
    /// host's image storage under `tag`, then removes the archive. skopeo (when
    /// present) copies straight into the runtime's storage transport; the
    /// fallback uses `<bin> load` + `<bin> tag` (both podman and docker spell
    /// these the same, but only skopeo avoids the oci-archive-vs-docker-archive
    /// load quirk — recommend skopeo on docker hosts).
    pub(super) fn load_archive_cmd(&self, archive: &str, tag: &str) -> Result<String, String> {
        let bin = self.runtime_bin()?;
        let transport = self.storage_transport()?;
        Ok(format!(
            "if command -v skopeo >/dev/null 2>&1; then \
               skopeo copy oci-archive:{archive} {transport}:{tag}; \
             else \
               ref=$({bin} load -i {archive} | sed -n 's/^Loaded image[^:]*: *//p' | tail -1); \
               [ -n \"$ref\" ] && {bin} tag \"$ref\" {tag}; \
             fi && rm -f {archive}"
        ))
    }

    /// Fetch the raw manifest (index) for `reference` and the sha256 of those
    /// bytes, trying local skopeo → local podman → remote skopeo → remote
    /// podman. The digest-of-document IS the (list) digest.
    fn fetch_manifest(&self, reference: &ImageRef) -> Result<(String, Digest), String> {
        let target = reference.pinned().unwrap_or_else(|| reference.name_tag());
        let name_tag = reference.name_tag();
        // Local container storage FIRST — the fully-local path (a `just
        // image-build`-loaded image resolves with NO registry). skopeo must be
        // told the rootless podman store explicitly ([driver@graphroot+runroot]);
        // the bare `containers-storage:` transport hits the ROOT store. docker's
        // daemon transport is unambiguous. Both miss for a genuinely-remote ref
        // and fall through to the registry.
        let mut attempts: Vec<(bool, String)> = Vec::new();
        if let Some(cs) = local_containers_storage_prefix() {
            attempts.push((true, format!("skopeo inspect --raw {cs}{name_tag}")));
        }
        attempts.push((
            true,
            format!("skopeo inspect --raw docker-daemon:{name_tag}"),
        ));
        attempts.push((true, format!("skopeo inspect --raw docker://{target}")));
        attempts.push((true, format!("podman manifest inspect docker://{target}")));
        attempts.push((false, format!("skopeo inspect --raw docker://{target}")));
        attempts.push((false, format!("podman manifest inspect docker://{target}")));
        let mut last_err = String::from("no manifest tool (skopeo/podman) available");
        for (local, cmd) in &attempts {
            let run = if *local {
                Self::exec_local(cmd, Duration::from_secs(60))
            } else {
                self.exec(cmd, Duration::from_secs(60))
            };
            match run {
                Ok(o) if o.ok && !o.stdout.trim().is_empty() => {
                    let json = o.stdout;
                    let digest = if *local {
                        sha256_local(&json)?
                    } else {
                        self.sha256_remote(&json)?
                    };
                    return Ok((json, digest));
                }
                Ok(o) => last_err = o.msg("inspect"),
                Err(f) => last_err = f.msg("inspect"),
            }
        }
        Err(format!("manifest inspect {target}: {last_err}"))
    }

    /// sha256 of a string via the host's `sha256sum` (stdin pipe).
    fn sha256_remote(&self, content: &str) -> Result<Digest, String> {
        // Content is JSON — safe to heredoc-quote via sh single quotes after
        // escaping. Use base64 to dodge quoting entirely.
        use std::io::Write;
        use std::process::{Command, Stdio};
        let argv = self.placement.control_argv(&[
            "sh".to_string(),
            "-lc".to_string(),
            "sha256sum | awk '{print $1}'".to_string(),
        ]);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("sha256: spawn: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("sha256: no stdin")?
            .write_all(content.as_bytes())
            .map_err(|e| format!("sha256: write: {e}"))?;
        let out = child
            .wait_with_output()
            .map_err(|e| format!("sha256: wait: {e}"))?;
        Digest::from_hex(String::from_utf8_lossy(&out.stdout).trim())
    }
}

/// sha256 of a local FILE via `sha256sum` (streaming; archives are GBs).
fn sha256_file_local(path: &std::path::Path) -> Result<Digest, String> {
    let label = format!("sha256sum {}", path.display());
    let out = exec_argv(
        &["sha256sum".to_string(), path.to_string_lossy().into_owned()],
        Duration::from_secs(600),
    )
    .map_err(|f| f.msg(&label))?;
    if !out.ok {
        return Err(out.msg(&label));
    }
    Digest::from_hex(out.stdout.split_whitespace().next().unwrap_or(""))
}

/// sha256 of a string via the local `sha256sum` (also the cloud adapter's
/// pseudo-digest source, so it's visible to sibling submodules).
pub(super) fn sha256_local(content: &str) -> Result<Digest, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("sh")
        .args(["-c", "sha256sum | awk '{print $1}'"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("sha256sum: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("sha256sum: no stdin")?
        .write_all(content.as_bytes())
        .map_err(|e| format!("sha256sum write: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("sha256sum wait: {e}"))?;
    Digest::from_hex(String::from_utf8_lossy(&out.stdout).trim())
}

/// Distro-detected runtime install (only ever reached with consent granted).
const INSTALL_PODMAN_SCRIPT: &str = r#"
set -eu
. /etc/os-release 2>/dev/null || true
if command -v apt >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  sudo -n apt-get update -y && sudo -n apt-get install -y podman uidmap slirp4netns
elif command -v dnf >/dev/null 2>&1; then
  sudo -n dnf install -y podman
elif command -v apk >/dev/null 2>&1; then
  sudo -n apk add podman
elif command -v pacman >/dev/null 2>&1; then
  sudo -n pacman -Sy --noconfirm podman
else
  echo "no supported package manager" >&2; exit 1
fi
# Rootless niceties: keep the user's services alive; check subuid ranges.
command -v loginctl >/dev/null 2>&1 && sudo -n loginctl enable-linger "$(id -un)" 2>/dev/null || true
grep -q "^$(id -un):" /etc/subuid 2>/dev/null || \
  sudo -n usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "$(id -un)" 2>/dev/null || true
podman --version
"#;

impl HostRunner for OciRunner {
    fn connect(&mut self) -> Result<(), ClassifiedErr> {
        if self.is_local() {
            return Ok(());
        }
        // A wedged ControlMaster makes every exec fail identically — check it
        // (and clear a dead socket) before the connect proves reachability.
        retry::master_hygiene(&self.placement);
        // Warm the multiplexed master; one cheap exec proves reachability.
        match self.exec("true", Duration::from_secs(30)) {
            Ok(o) if o.ok => Ok(()),
            Ok(o) => Err(o.cerr("connect")),
            Err(f) => Err(f.cerr("connect")),
        }
    }

    fn probe(&mut self) -> Result<HostCaps, ClassifiedErr> {
        let out = self
            .exec(PROBE_SCRIPT, Duration::from_secs(60))
            .map_err(|f| f.cerr("probe"))?;
        if !out.ok {
            return Err(out.cerr("probe"));
        }
        let mut caps = HostCaps::parse_probe(&out.stdout).map_err(|e| format!("probe: {e}"))?;
        // A local host can't SshStream to itself; local delivery is a plain
        // pull / local storage share.
        if self.is_local() {
            caps.delivery.remove(&DeliveryCap::SshStream);
            caps.delivery.remove(&DeliveryCap::Rsync);
        }
        self.caps = Some(caps.clone());
        Ok(caps)
    }

    fn probe_headroom(&mut self) -> Result<thegn_core::host_probe::Headroom, ClassifiedErr> {
        let out = self
            .exec(HEADROOM_SCRIPT, Duration::from_secs(15))
            .map_err(|f| f.cerr("headroom"))?;
        if !out.ok {
            return Err(out.cerr("headroom"));
        }
        thegn_core::host_probe::parse_headroom(&out.stdout)
            .map_err(|e| format!("headroom: {e}").into())
    }

    fn install_runtime(
        &mut self,
        kind: RuntimeKind,
        note: &mut dyn FnMut(String),
    ) -> Result<RuntimeInfo, ClassifiedErr> {
        if kind != RuntimeKind::Podman {
            return Err("only podman bootstrap is supported".into());
        }
        note("installing podman via the detected package manager".into());
        let out = self
            .exec(INSTALL_PODMAN_SCRIPT, Duration::from_secs(900))
            .map_err(|f| f.cerr("install"))?;
        if !out.ok {
            return Err(out.cerr("install"));
        }
        note("verifying the installed runtime".into());
        let caps = self.probe()?;
        caps.runtime
            .ok_or_else(|| "install completed but podman still not found".into())
    }

    fn resolve_image(&mut self, reference: &ImageRef) -> Result<ResolvedImage, ClassifiedErr> {
        let arch = self.caps.as_ref().map(|c| c.arch).unwrap_or(Arch::Amd64);
        let (json, self_digest) = self.fetch_manifest(reference)?;
        // A digest-pinned reference must hash to its pin.
        if let Some(pin) = &reference.manifest_list_digest
            && pin != &self_digest
        {
            return Err(format!(
                "manifest digest mismatch: pinned {}, fetched {}",
                pin.short(),
                self_digest.short()
            )
            .into());
        }
        ResolvedImage::parse_manifest_index(reference, &json, self_digest, arch)
            .map_err(ClassifiedErr::from)
    }

    fn image_present(&mut self, image: &ImageRef, digest: &Digest) -> Result<bool, ClassifiedErr> {
        let bin = self.runtime_bin()?;
        let target = format!("{}@{}", image.name, digest);
        // The managed digest tag is the uniform run reference (stream-loaded
        // images have no name@digest association); either form counts, but a
        // missing tag on a pulled image is repaired so spawn always works.
        let tag = thegn_core::image::managed_tag(digest);
        let tag_exists = self.exists_test("image", &tag)?;
        let target_exists = self.exists_test("image", &target)?;
        match self.exec(
            &format!(
                "if {tag_exists}; then exit 0; fi; \
                 if {target_exists}; then {bin} tag {target} {tag}; exit 0; fi; \
                 exit 1"
            ),
            Duration::from_secs(30),
        ) {
            Ok(o) => Ok(o.ok),
            Err(f) => Err(f.cerr("image check")),
        }
    }

    fn deliver(
        &mut self,
        strategy: DeliveryStrategy,
        image: &ImageRef,
        digest: &Digest,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Digest, ClassifiedErr> {
        let bin = self.runtime_bin()?;
        let target = format!("{}@{}", image.name, digest);
        match strategy {
            DeliveryStrategy::RegistryPull => {
                // Pull the per-arch digest exactly; podman verifies content.
                let out = self
                    .exec(
                        &format!("{bin} pull -q {target}"),
                        Duration::from_secs(1800),
                    )
                    .map_err(|f| f.cerr("pull"))?;
                if !out.ok {
                    return Err(out.cerr("pull"));
                }
            }
            DeliveryStrategy::SkopeoRemoteCopy => {
                let transport = self.storage_transport()?;
                let out = self
                    .exec(
                        &format!(
                            "skopeo copy --retry-times 3 docker://{target} \
                             {transport}:{}",
                            image.name_tag()
                        ),
                        Duration::from_secs(1800),
                    )
                    .map_err(|f| f.cerr("skopeo copy"))?;
                if !out.ok {
                    return Err(out.cerr("skopeo copy"));
                }
            }
            DeliveryStrategy::SshStream { rsync } => {
                deliver::ssh_stream(self, image, digest, rsync, progress)?;
            }
            DeliveryStrategy::RemoteBuild => {
                return Err("remote build delivery not implemented yet".into());
            }
            DeliveryStrategy::ProviderTemplate => {
                return Err("provider-template delivery belongs to the cloud adapter".into());
            }
        }
        // Verify before anything boots from it (also repairs the managed tag
        // for the pull/skopeo strategies).
        if self.image_present(image, digest)? {
            Ok(digest.clone())
        } else {
            Err(format!(
                "delivered but {} not present on the host (digest mismatch?)",
                digest.short()
            )
            .into())
        }
    }

    fn seed_volume(
        &mut self,
        spec: &VolumeSpec,
        image: &ImageRef,
        digest: &Digest,
    ) -> Result<(), ClassifiedErr> {
        let bin = self.runtime_bin()?;
        // Idempotent: an existing volume is a seeded volume (copy-up happened
        // on its first mount).
        if self
            .exec(
                &self.exists_test("volume", &spec.name)?,
                Duration::from_secs(30),
            )
            .map(|o| o.ok)
            .unwrap_or(false)
        {
            return Ok(());
        }
        match &spec.seed {
            VolumeSeed::ImageCopyUp => {
                let _ = image;
                let target = thegn_core::image::managed_tag(digest);
                let cmd = format!(
                    "{bin} volume create --label thegn.managed=true \
                       --label thegn.volume.role={} {} >/dev/null && \
                     {bin} run --rm --label thegn.managed=true \
                       -v {}:{} {target} true",
                    spec.name, spec.name, spec.name, spec.dest
                );
                let label = format!("volume seed {}", spec.name);
                let out = self
                    .exec(&cmd, Duration::from_secs(1800))
                    .map_err(|f| f.cerr(&label))?;
                if !out.ok {
                    // Never leave a half-seeded volume: a later run would see
                    // `volume exists` and trust it.
                    let _ = self.exec(
                        &format!("{bin} volume rm -f {}", spec.name),
                        Duration::from_secs(60),
                    );
                    return Err(out.cerr(&label));
                }
                Ok(())
            }
            VolumeSeed::Tarball { .. } => Err("tarball volume seeding not implemented yet".into()),
        }
    }

    fn oci_url(&self) -> Option<String> {
        // No URL for local; none for iroh either — the forwarded port dies
        // with this runner, so a pinned URL would outlive its tunnel.
        if self.is_local() || self.tunnel.is_some() {
            return None;
        }
        let socket = self.caps.as_ref()?.runtime.as_ref()?.socket.as_deref()?;
        let Placement::Ssh(p) = &self.placement else {
            return None;
        };
        Some(format!("ssh://{}:{}{}", p.host, p.port, socket))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_runner_connects_trivially_and_ssh_urls_need_sockets() {
        let mut local = OciRunner::new(Placement::Local);
        assert!(local.connect().is_ok());
        assert_eq!(local.oci_url(), None);
    }

    #[test]
    fn probe_script_contract_matches_core_parser_on_this_machine() {
        // The probe runs LOCALLY here — asserting the script's output parses
        // keeps the svc script and the core parser contract in lockstep.
        let mut r = OciRunner::new(Placement::Local);
        let caps = r.probe().expect("local probe parses");
        assert_eq!(caps.os, "linux");
        // Local hosts never stream to themselves.
        assert!(!caps.delivery.contains(&DeliveryCap::SshStream));
    }

    #[test]
    fn headroom_script_contract_matches_core_parser_on_this_machine() {
        // Runs LOCALLY: keeps HEADROOM_SCRIPT and the core parser in lockstep.
        let mut r = OciRunner::new(Placement::Local);
        let h = r.probe_headroom().expect("local headroom parses");
        assert!(h.cpus >= 1);
        assert!(h.mem_total_kb > 0);
        assert!(h.mem_available_kb > 0);
    }

    #[test]
    fn probe_script_reports_machine_size_keys() {
        // The extended probe carries the size hints the capacity layer reads.
        let mut r = OciRunner::new(Placement::Local);
        let caps = r.probe().expect("local probe parses");
        assert!(caps.nproc.unwrap_or(0) >= 1);
        assert!(caps.mem_total_kb.unwrap_or(0) > 0);
    }

    #[test]
    fn runner_for_dispatches_by_reach() {
        assert!(runner_for(&Reach::Local).is_ok());
        assert!(
            runner_for(&Reach::Iroh(IrohReach {
                ticket: "t".into(),
                ssh_port: 22,
                user: "u".into()
            }))
            .is_err()
        );
        // Cloud lowers to the provider adapter (known providers only).
        assert!(
            runner_for(&Reach::Cloud(thegn_core::host::CloudReach {
                provider: "sprites".into(),
                api_base: String::new(),
                api_key_env: String::new(),
                template: "base".into()
            }))
            .is_ok()
        );
        assert!(
            runner_for(&Reach::Cloud(thegn_core::host::CloudReach {
                provider: "nimbus".into(),
                api_base: String::new(),
                api_key_env: String::new(),
                template: "base".into()
            }))
            .is_err()
        );
    }

    #[test]
    fn pipe_and_container_helpers_shape_argv_locally() {
        // Local placement makes the pipe run end-to-end on this machine:
        // tar a staging dir into a destination dir via the exec channel.
        let src = std::env::temp_dir().join(format!("sz-pipe-src-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("sz-pipe-dst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("hello.txt"), b"hi").unwrap();
        let r = OciRunner::new(Placement::Local);
        let local = vec![
            "tar".to_string(),
            "-C".to_string(),
            src.to_string_lossy().into_owned(),
            "-cf".to_string(),
            "-".to_string(),
            ".".to_string(),
        ];
        r.pipe_local_to_host(
            &local,
            &format!(
                "mkdir -p {} && tar -xf - -C {}",
                dst.display(),
                dst.display()
            ),
            Duration::from_secs(30),
        )
        .expect("pipe succeeds");
        assert_eq!(std::fs::read(dst.join("hello.txt")).unwrap(), b"hi");
        // A failing host command surfaces the classified failure with its
        // label and exit code (describe_exec_failure shapes the message).
        let err = r
            .pipe_local_to_host(&local, "exit 3", Duration::from_secs(10))
            .unwrap_err();
        assert!(
            err.contains("pipe: host cmd") && err.contains("exit 3"),
            "{err}"
        );
    }

    #[test]
    fn exec_failure_messages_name_the_real_cause() {
        // A transport drop (255, silent) names the drop — never "(no output)".
        let drop = ExecOut {
            ok: false,
            code: Some(255),
            stdout: String::new(),
            stderr: String::new(),
        };
        let msg = drop.msg("connect");
        assert!(msg.contains("transport dropped"), "{msg}");
        assert!(!msg.contains("(no output)"), "{msg}");
        assert_eq!(drop.class(), ErrorClass::Transient);
        // A real remote failure carries the exit code + stderr tail.
        let real = ExecOut {
            ok: false,
            code: Some(1),
            stdout: String::new(),
            stderr: "a\nthe cause\n\n".into(),
        };
        let msg = real.msg("probe");
        assert!(msg.contains("exit 1") && msg.contains("the cause"), "{msg}");
        assert_eq!(real.class(), ErrorClass::Permanent);
        // Timeouts are transient; spawn failures are not.
        let to = ExecFail::Timeout { secs: 30 };
        assert!(to.msg("pull").contains("timed out after 30s"));
        assert_eq!(to.class(), ErrorClass::Transient);
        assert_eq!(
            ExecFail::Spawn("ssh: not found".into()).class(),
            ErrorClass::Permanent
        );
    }

    #[test]
    fn sha256_local_matches_known_vector() {
        let d = sha256_local("hello\n").unwrap();
        assert_eq!(
            d.as_str(),
            "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
        );
    }

    /// Build a runner whose probed runtime is fixed by a synthetic probe line,
    /// so the command-spelling helpers can be tested without a real host.
    fn runner_with_runtime(probe: &str) -> OciRunner {
        let caps = HostCaps::parse_probe(probe).expect("probe parses");
        let mut r = OciRunner::new(Placement::Local);
        r.caps = Some(caps);
        r
    }

    #[test]
    fn oci_command_spelling_is_runtime_aware() {
        // A docker host answers `docker image inspect` / `docker-daemon:` — the
        // `podman … exists` sugar and `containers-storage:` transport do NOT
        // exist there, so a podman-hardcoded command would fail provisioning.
        let podman = runner_with_runtime("ARCH=x86_64\nOS=linux\nPODMAN=4.9.3\n");
        let docker = runner_with_runtime("ARCH=x86_64\nOS=linux\nDOCKER=24.0.5\n");

        assert_eq!(podman.runtime_bin().unwrap(), "podman");
        assert_eq!(docker.runtime_bin().unwrap(), "docker");
        assert_eq!(podman.storage_transport().unwrap(), "containers-storage");
        assert_eq!(docker.storage_transport().unwrap(), "docker-daemon");

        assert_eq!(
            podman.exists_test("image", "img").unwrap(),
            "podman image exists img"
        );
        assert_eq!(
            docker.exists_test("image", "img").unwrap(),
            "docker image inspect img >/dev/null 2>&1"
        );
        assert_eq!(
            docker.exists_test("volume", "vol").unwrap(),
            "docker volume inspect vol >/dev/null 2>&1"
        );

        let pload = podman.load_archive_cmd("/a.tar", "localhost/x:t").unwrap();
        assert!(
            pload.contains("containers-storage:localhost/x:t"),
            "{pload}"
        );
        assert!(pload.contains("podman load -i /a.tar"), "{pload}");
        assert!(pload.contains("podman tag"), "{pload}");

        let dload = docker.load_archive_cmd("/a.tar", "localhost/x:t").unwrap();
        assert!(dload.contains("docker-daemon:localhost/x:t"), "{dload}");
        assert!(dload.contains("docker load -i /a.tar"), "{dload}");
        assert!(dload.contains("docker tag"), "{dload}");
        assert!(
            !dload.contains("podman"),
            "no podman-isms on docker: {dload}"
        );
    }
}
