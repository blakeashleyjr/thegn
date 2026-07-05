//! Host provisioning **runners**: the impure executors behind the pure
//! [`superzej_core::host_machine`] state machine. A runner turns each
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

use superzej_core::host::{
    Arch, DeliveryCap, HostCaps, IrohReach, Reach, RuntimeInfo, RuntimeKind, VolumeSeed, VolumeSpec,
};
use superzej_core::image::{DeliveryStrategy, Digest, ImageRef, LocalCaps, ResolvedImage};
use superzej_core::placement::Placement;

mod cloud;
mod deliver;

pub use cloud::cloud_runner_for;
pub use deliver::stream_archive_over_ssh;

/// Everything the host-flow driver needs to execute effects against one host.
/// Implementations own their per-step deadlines; errors are plain strings that
/// flow into `HostEvent::*Failed` (and from there into actionable failures).
pub trait HostRunner: Send {
    /// Open (or re-verify) the control channel. For ssh this warms the
    /// ControlMaster so every later exec rides one TCP/auth handshake.
    fn connect(&mut self) -> Result<(), String>;
    /// Run the single-shot probe and parse its `KEY=VALUE` contract.
    fn probe(&mut self) -> Result<HostCaps, String>;
    /// Bootstrap a runtime. The driver only calls this with consent granted.
    fn install_runtime(
        &mut self,
        kind: RuntimeKind,
        note: &mut dyn FnMut(String),
    ) -> Result<RuntimeInfo, String>;
    /// Resolve the image reference to its per-arch digests.
    fn resolve_image(&mut self, reference: &ImageRef) -> Result<ResolvedImage, String>;
    /// Is `name@digest` already in the host's image storage?
    fn image_present(&mut self, image: &ImageRef, digest: &Digest) -> Result<bool, String>;
    /// Execute one delivery strategy; returns the digest VERIFIED on the host.
    fn deliver(
        &mut self,
        strategy: DeliveryStrategy,
        image: &ImageRef,
        digest: &Digest,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Digest, String>;
    /// Idempotently seed one warm volume (exists ⇒ no-op success).
    fn seed_volume(
        &mut self,
        spec: &VolumeSpec,
        image: &ImageRef,
        digest: &Digest,
    ) -> Result<(), String>;
    /// The remote OCI daemon URL sandbox spawn should pin (`None` ⇒ the
    /// placement transport wraps the whole argv, as today).
    fn oci_url(&self) -> Option<String>;
    /// One cheap live-resources sample (the placement engine's measured
    /// layer). Default: unsupported — cloud runners have no shell to ask.
    fn probe_headroom(&mut self) -> Result<superzej_core::host_probe::Headroom, String> {
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
fn lower_iroh(
    i: &IrohReach,
) -> Result<(IrohTunnel, superzej_core::placement::SshPlacement), String> {
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
    let alias = format!(
        "superzej-iroh-{}",
        superzej_core::util::short_hash(&i.ticket, 8)
    );
    let placement = superzej_core::placement::SshPlacement {
        host: format!("{}@127.0.0.1", i.user),
        port,
        forward_agent: false,
        kind: superzej_core::placement::TransportKind::Ssh,
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
        .map(|(ok, _, _)| ok)
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
/// cheap exec, parsed by the pure `superzej_core::host_probe::parse_headroom`
/// (extend BOTH together — the contract test below pins agreement).
const HEADROOM_SCRIPT: &str = r#"
set -u
echo "NPROC=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"
awk '/MemTotal/ {print "MEM_TOTAL_KB=" $2} /MemAvailable/ {print "MEM_AVAIL_KB=" $2}' /proc/meminfo 2>/dev/null
awk '{printf "LOAD1_MILLI=%d
", $1 * 1000}' /proc/loadavg 2>/dev/null
df -kP "${HOME:-/}" 2>/dev/null | awk 'NR==2 {print "DISK_FREE=" $4 * 1024}'
command -v podman >/dev/null 2>&1 && echo "CONTAINERS=$(podman ps -q 2>/dev/null | wc -l | tr -d ' ')"
true
"#;

/// Run `argv` to completion with a hard deadline, capturing stdout + stderr.
/// `Err` on spawn failure or timeout (child killed + reaped).
fn exec_argv(argv: &[String], timeout: Duration) -> Result<(bool, String, String), String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", argv[0]))?;
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
                return Ok((status.success(), out, err));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "timed out after {}s: {}",
                    timeout.as_secs(),
                    argv.join(" ")
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    }
}

/// Shorten a stderr blob into a one-line error tail.
fn err_tail(err: &str) -> String {
    err.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("(no output)")
        .trim()
        .to_string()
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

    /// Run a shell command on the host with a deadline.
    fn exec(&self, cmd: &str, timeout: Duration) -> Result<(bool, String, String), String> {
        exec_argv(&self.control_shell_argv(cmd), timeout)
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
                superzej_core::util::sh_quote(cmd)
            ),
            timeout,
        )
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
                    return Err(format!("pipe: host cmd failed: {}", err_tail(&err)));
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
            superzej_core::util::sh_quote(&format!("mkdir -p {dest} && tar -xf - -C {dest}"))
        );
        self.pipe_local_to_host(&local, &host_cmd, timeout)
    }

    /// Run a shell command LOCALLY with a deadline.
    fn exec_local(cmd: &str, timeout: Duration) -> Result<(bool, String, String), String> {
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

    /// Fetch the raw manifest (index) for `reference` and the sha256 of those
    /// bytes, trying local skopeo → local podman → remote skopeo → remote
    /// podman. The digest-of-document IS the (list) digest.
    fn fetch_manifest(&self, reference: &ImageRef) -> Result<(String, Digest), String> {
        let target = reference.pinned().unwrap_or_else(|| reference.name_tag());
        let attempts: [(bool, String); 4] = [
            (true, format!("skopeo inspect --raw docker://{target}")),
            (true, format!("podman manifest inspect docker://{target}")),
            (false, format!("skopeo inspect --raw docker://{target}")),
            (false, format!("podman manifest inspect docker://{target}")),
        ];
        let mut last_err = String::from("no manifest tool (skopeo/podman) available");
        for (local, cmd) in &attempts {
            let run = if *local {
                Self::exec_local(cmd, Duration::from_secs(60))
            } else {
                self.exec(cmd, Duration::from_secs(60))
            };
            match run {
                Ok((true, json, _)) if !json.trim().is_empty() => {
                    let digest = if *local {
                        sha256_local(&json)?
                    } else {
                        self.sha256_remote(&json)?
                    };
                    return Ok((json, digest));
                }
                Ok((_, _, err)) => last_err = err_tail(&err),
                Err(e) => last_err = e,
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
    let (ok, out, err) = exec_argv(
        &["sha256sum".to_string(), path.to_string_lossy().into_owned()],
        Duration::from_secs(600),
    )?;
    if !ok {
        return Err(format!("sha256sum {}: {}", path.display(), err_tail(&err)));
    }
    Digest::from_hex(out.split_whitespace().next().unwrap_or(""))
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
    fn connect(&mut self) -> Result<(), String> {
        if self.is_local() {
            return Ok(());
        }
        // Warm the multiplexed master; one cheap exec proves reachability.
        match self.exec("true", Duration::from_secs(30)) {
            Ok((true, _, _)) => Ok(()),
            Ok((false, _, err)) => Err(format!("connect: {}", err_tail(&err))),
            Err(e) => Err(format!("connect: {e}")),
        }
    }

    fn probe(&mut self) -> Result<HostCaps, String> {
        let (ok, out, err) = self
            .exec(PROBE_SCRIPT, Duration::from_secs(60))
            .map_err(|e| format!("probe: {e}"))?;
        if !ok {
            return Err(format!("probe: {}", err_tail(&err)));
        }
        let mut caps = HostCaps::parse_probe(&out).map_err(|e| format!("probe: {e}"))?;
        // A local host can't SshStream to itself; local delivery is a plain
        // pull / local storage share.
        if self.is_local() {
            caps.delivery.remove(&DeliveryCap::SshStream);
            caps.delivery.remove(&DeliveryCap::Rsync);
        }
        self.caps = Some(caps.clone());
        Ok(caps)
    }

    fn probe_headroom(&mut self) -> Result<superzej_core::host_probe::Headroom, String> {
        let (ok, out, err) = self
            .exec(HEADROOM_SCRIPT, Duration::from_secs(15))
            .map_err(|e| format!("headroom: {e}"))?;
        if !ok {
            return Err(format!("headroom: {}", err_tail(&err)));
        }
        superzej_core::host_probe::parse_headroom(&out).map_err(|e| format!("headroom: {e}"))
    }

    fn install_runtime(
        &mut self,
        kind: RuntimeKind,
        note: &mut dyn FnMut(String),
    ) -> Result<RuntimeInfo, String> {
        if kind != RuntimeKind::Podman {
            return Err("only podman bootstrap is supported".into());
        }
        note("installing podman via the detected package manager".into());
        let (ok, _out, err) = self
            .exec(INSTALL_PODMAN_SCRIPT, Duration::from_secs(900))
            .map_err(|e| format!("install: {e}"))?;
        if !ok {
            return Err(format!("install: {}", err_tail(&err)));
        }
        note("verifying the installed runtime".into());
        let caps = self.probe()?;
        caps.runtime
            .ok_or_else(|| "install completed but podman still not found".to_string())
    }

    fn resolve_image(&mut self, reference: &ImageRef) -> Result<ResolvedImage, String> {
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
            ));
        }
        ResolvedImage::parse_manifest_index(reference, &json, self_digest, arch)
    }

    fn image_present(&mut self, image: &ImageRef, digest: &Digest) -> Result<bool, String> {
        let bin = self.runtime_bin()?;
        let target = format!("{}@{}", image.name, digest);
        // The managed digest tag is the uniform run reference (stream-loaded
        // images have no name@digest association); either form counts, but a
        // missing tag on a pulled image is repaired so spawn always works.
        let tag = superzej_core::image::managed_tag(digest);
        match self.exec(
            &format!(
                "if {bin} image exists {tag}; then exit 0; fi; \
                 if {bin} image exists {target}; then {bin} tag {target} {tag}; exit 0; fi; \
                 exit 1"
            ),
            Duration::from_secs(30),
        ) {
            Ok((ok, _, _)) => Ok(ok),
            Err(e) => Err(format!("image check: {e}")),
        }
    }

    fn deliver(
        &mut self,
        strategy: DeliveryStrategy,
        image: &ImageRef,
        digest: &Digest,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Digest, String> {
        let bin = self.runtime_bin()?;
        let target = format!("{}@{}", image.name, digest);
        match strategy {
            DeliveryStrategy::RegistryPull => {
                // Pull the per-arch digest exactly; podman verifies content.
                let (ok, _, err) = self
                    .exec(
                        &format!("{bin} pull -q {target}"),
                        Duration::from_secs(1800),
                    )
                    .map_err(|e| format!("pull: {e}"))?;
                if !ok {
                    return Err(format!("pull: {}", err_tail(&err)));
                }
            }
            DeliveryStrategy::SkopeoRemoteCopy => {
                let (ok, _, err) = self
                    .exec(
                        &format!(
                            "skopeo copy --retry-times 3 docker://{target} \
                             containers-storage:{}",
                            image.name_tag()
                        ),
                        Duration::from_secs(1800),
                    )
                    .map_err(|e| format!("skopeo copy: {e}"))?;
                if !ok {
                    return Err(format!("skopeo copy: {}", err_tail(&err)));
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
            ))
        }
    }

    fn seed_volume(
        &mut self,
        spec: &VolumeSpec,
        image: &ImageRef,
        digest: &Digest,
    ) -> Result<(), String> {
        let bin = self.runtime_bin()?;
        // Idempotent: an existing volume is a seeded volume (copy-up happened
        // on its first mount).
        if let Ok((true, _, _)) = self.exec(
            &format!("{bin} volume exists {}", spec.name),
            Duration::from_secs(30),
        ) {
            return Ok(());
        }
        match &spec.seed {
            VolumeSeed::ImageCopyUp => {
                let _ = image;
                let target = superzej_core::image::managed_tag(digest);
                let cmd = format!(
                    "{bin} volume create --label superzej.managed=true \
                       --label superzej.volume.role={} {} >/dev/null && \
                     {bin} run --rm --label superzej.managed=true \
                       -v {}:{} {target} true",
                    spec.name, spec.name, spec.name, spec.dest
                );
                let (ok, _, err) = self
                    .exec(&cmd, Duration::from_secs(1800))
                    .map_err(|e| format!("volume seed: {e}"))?;
                if !ok {
                    // Never leave a half-seeded volume: a later run would see
                    // `volume exists` and trust it.
                    let _ = self.exec(
                        &format!("{bin} volume rm -f {}", spec.name),
                        Duration::from_secs(60),
                    );
                    return Err(format!("volume seed {}: {}", spec.name, err_tail(&err)));
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
            runner_for(&Reach::Cloud(superzej_core::host::CloudReach {
                provider: "sprites".into(),
                api_base: String::new(),
                api_key_env: String::new(),
                template: "base".into()
            }))
            .is_ok()
        );
        assert!(
            runner_for(&Reach::Cloud(superzej_core::host::CloudReach {
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
        // A failing host command surfaces its stderr tail.
        let err = r
            .pipe_local_to_host(&local, "exit 3", Duration::from_secs(10))
            .unwrap_err();
        assert!(err.contains("host cmd failed"), "{err}");
    }

    #[test]
    fn err_tail_takes_last_nonempty_line() {
        assert_eq!(err_tail("a\nb\n\n"), "b");
        assert_eq!(err_tail(""), "(no output)");
    }

    #[test]
    fn sha256_local_matches_known_vector() {
        let d = sha256_local("hello\n").unwrap();
        assert_eq!(
            d.as_str(),
            "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
        );
    }
}
