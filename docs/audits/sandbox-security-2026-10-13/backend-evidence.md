# Backend evidence for the sandbox security audit

Audit date: 2026-10-13. Source revision: `38e09ded78652c6cf56b0e3a7b6a7c16d18dcd95`.

This document records backend implementation facts, not runtime certification. All reads and artifacts were made in `/home/blake/code/thegn-audit-backends-20261013`; the shared original checkout was not edited. No production source/API changes were made. The accompanying `backend-probe.py` compiles extracted production helpers with Rust, replaces only a tracing macro, and uses a controlled loopback UDP responder. It demonstrates helper behavior independently of container availability. Real backend canary results belong in the runtime validation artifact.

## Severity-ranked findings

### B01 — High: persistent container reuse does not reconcile the security policy

Affected: every OCI launch using `ensure`, including panes and agents; Apple has a separate parser with the same source-set comparison.

`container_status` builds a required set of mount **sources**, then reads only running state, a sealed-home label, and live bind **sources** (`crates/thegn-core/src/sandbox.rs:2015`, `:2028`). `judge_inspect` checks the label and calls `mounts_match` (`:1916`); `ensure` returns success immediately when this comparison passes (`:2105`). Nonsealed specs permit extra old binds; sealed specs require source-set equality, with a Nix exception (`:1896`). Neither comparison includes mount destinations or writability, network mode/DNS/VPN, image, OCI runtime, capabilities, process limits, environment, or root writability.

Reproduction evidence: the extracted helpers report `reusable=true` for the inspected input shared by an existing hardened NAT container and a requested hardened `network=none` container with the same mounts; they also accept an extra removed-secret bind for a nonsealed request. This proves the reconciliation gap. A real container transition was not executed in this evidence task.

Impact: a newly narrowed request may enter a previously broader running container. Changing only `network=none`, rw→ro mounts, or OCI runtime does not force replacement. The reported class can be derived from a new spec while `exec` enters an old runtime. Sealed and sealed-tunnel have the same sealed marker, so changes between them must also reconcile actual network attachment. Recreate from a complete policy fingerprint and verify the live runtime policy before reuse; workspace data must survive recreation independently.

### B02 — High: hostname settings are DNS filtering, with silent unsupported paths and no packet boundary

Affected: hostname policies on all backends. Local OCI attempts DNS interception; bwrap/systemd/host/native Windows do not use it. Remote placement and VPN sidecar joins skip ordinary OCI interception.

OCI creation conditionally adds `--dns 127.0.0.1:<port>` only for local placements with network enabled, no sidecar join, and a nonempty list (`crates/thegn-core/src/sandbox.rs:3214`). Binding failure yields `None` and no DNS option, with no policy refusal (`:3228`). Bwrap applies `--unshare-net` only for `Network::None` (`:2739`); systemd applies `PrivateNetwork=yes` only for `None` (`:2786`). Neither arm reads hostname allow/block lists. NAT is omitted in OCI arguments and therefore uses the runtime's default network (`:3195`); bwrap/systemd NAT and host both retain the host stack.

The DNS server receives UDP queries, obtains a name, and returns NXDOMAIN or forwards them (`crates/thegn-core/src/dns_filter.rs:123`). No connection/IP/port filter is installed by this path. Direct IP connections, caller-selected alternate DNS, and DNS-over-HTTPS therefore bypass **this mechanism** wherever the backend otherwise permits that traffic. The localhost filter address belongs to the launcher host; under a private OCI network namespace, container loopback is a different interface. Actual acceptance/routing of the `--dns` value must be runtime-tested and must not be counted as enforcement from argv alone.

Impact: `network_allow`, `network_block`, and repo “deny all egress” requests are not enforceable network grants. Repo empty/intersection-empty allow lists are encoded as `network_block += ["*"]` (`crates/thegn-core/src/config_resolve.rs:658`, `:692`), so this limitation also undermines an otherwise sound narrowing model. Only namespace `network=none` is a currently expressed full denial for fresh supported launches, subject to B01 and host/service socket exceptions. Reject unsupported destination policies and use a routed, enforced datapath for selective grants.

### B03 — High: DNS singleton retains the first policy for every later sandbox

Affected: local OCI hostname filtering and any other caller of the shared filter (including VPN filter-front callers).

`INSTANCE` is a process-wide `OnceLock<Mutex<Option<DnsFilter>>>`; when already set, `get_or_start` returns the stored port and discards the supplied policy (`crates/thegn-core/src/dns_filter.rs:81`). The server thread captures the first `DnsPolicy` (`:123`). There is no workspace/policy key and no policy update operation.

Reproduction: start with `allow=["allowed.test"]`, controlled localhost upstream, then request `block=["*"]`. Both calls return the same port; querying `allowed.test` returns rcode 0 via the first upstream. See `backend-probe-output.txt`. No external DNS was used.

Impact: opening an earlier permissive workspace can weaken the later workspace's requested DNS denial; order also creates unexpected overblocking. A shared event ring further loses workspace attribution. Use one immutable policy instance per policy identity/network boundary, with ownership and lifecycle records.

### B04 — High: default hardened exposes ambient home credentials and writable host execution state

Affected: Linux OCI/bwrap with default auto-caches/home injection; systemd ambient host home; config-specific exposures on other platforms. Linux/macOS ABI gating means the whole-home toolchain injection is withheld for Linux OCI guests on macOS (`crates/thegn-core/src/sandbox.rs:1112`).

Defaults are `hardened`, NAT, auto-caches on, mounts `~/.gitconfig:ro` and `~/.gnupg:rw`; environment passthrough includes `GH_TOKEN`, `GITHUB_TOKEN`, `ANTHROPIC_API_KEY`, `SSH_AUTH_SOCK`, and GPG settings (`crates/thegn-core/src/config.rs:4378`, `:4387`, `:4407`, `:4416`). The default file-access mode is `WorktreePlusCaches` (`:3915`). Home injection explicitly mounts the whole home (`crates/thegn-core/src/sandbox_mounts.rs:149`) and resolver config mounts can overmount a read-only parent as rw (`crates/thegn-core/src/sandbox.rs:1225`). SSH identity symlink targets outside home are explicitly brought back read-only for nonsealed local toolchain injection (`:1235`). Read-only secrets remain readable.

Additional rw carveouts include `.keychain` (scripts later sourced by host shells), `.claude-profiles`, `.claude/projects`, and provider config homes (`crates/thegn-core/src/sandbox_mounts.rs:294`, `:427`). Existing package/compiler caches are shared rw (`:164`). Host-side execution of tampered cache content or sourced keychain scripts is an integrity risk; this audit did not execute a host persistence payload.

Impact: “hardened” constrains many writes but does not provide home confidentiality or narrow credentials, and default network access permits exfiltration. It can mutate some host tool/credential state and caches. Keep this as an explicitly trusted convenience tier; secure untrusted defaults require a private home, independent workspace, dedicated caches and scoped credential grants. Sealed profiles intentionally remove ambient-home injection and deny protected mounts (`crates/thegn-core/src/sandbox.rs:1123`, `:1204`, `:667`); this is an existing protection, not a missing feature.

### B05 — High: live worktree mounts grant writable shared Git metadata

Affected: worktree/custom filesystem modes on OCI, bwrap, systemd; host processes already have ambient access.

Resolution discovers `git rev-parse --git-common-dir`, adds the worktree rw, and adds the shared common Git directory rw (`crates/thegn-core/src/sandbox.rs:1044`). Only shared `.git/config` is pinned read-only if it exists locally (`:1068`). Objects, refs, per-worktree state, and hooks remain writable. Git environment scrubbing (`:1335`) prevents accidental targeting but does not revoke path access.

Impact: sandbox commits/refs mutate host repository state; an untrusted process can alter shared hooks that host Git later executes or modify sibling worktree metadata. The main checkout's source files may be read via hardened's home mount even where its source directory is not rw. A copied linked-worktree directory preserves this exposure through `.git` pointers. The private-copy design must use independent Git metadata with no alternates/shared refs/hooks and explicit file/patch sync.

### B06 — High: enabled sandbox does not require a boundary by default

Affected: backend resolution/fallback; caller-specific floor behavior is documented in the launcher evidence.

Default chain ends with `host`, and includes rootful Podman before Docker/bwrap (`crates/thegn-core/src/config_defaults.rs:40`). Defaults are `on_missing=warn`, `isolation_floor=off`, `on_floor_miss=degrade` (`crates/thegn-core/src/config.rs:4425`). If an explicit runtime is unusable, chain-mode resolution walks alternatives and can end at host, even if the configured chain omits host (`crates/thegn-core/src/sandbox_backend.rs:104`, `:163`). Prompt is treated as warn by `on_missing` (`:356`). Local `Backend::None` returns no spec (`crates/thegn-core/src/sandbox.rs:1032`).

The floor helper can fail closed, but an off floor accepts every boundary; provider-managed placement bypasses the ranking by design (`crates/thegn-core/src/sandbox_floor.rs:84`). `agent_task_gate` allows a host execution when sandbox is off or establishment fails without a demanded floor (`:198`). Missing `runsc`/`krun` host requirements degrade to the daemon default (`crates/thegn-core/src/sandbox_runtime.rs:51`).

Impact: selecting sandbox enabled/hardened does not prove isolation. Warnings/observed labels help visibility but do not block execution. A secure request needs a mandatory non-host floor and a fail policy covering preparation, runtime recreation, and final spawn. Provider bypass must remain an explicit trust choice, not be labelled a passed ranked floor.

### B07 — High: reserved native Windows AppContainer is reported as available confinement

Affected: explicit native Windows `appcontainer` selection; not a Linux runtime finding.

Availability is simply local `cfg!(windows)` (`crates/thegn-core/src/sandbox_backend.rs:632`), generated argv is a plain user shell (`crates/thegn-core/src/sandbox.rs:2632`), truth reconciliation accepts the requested Windows label without observation (`crates/thegn-core/src/sandbox_truth.rs:191`), and capability ranking assigns the reserved AppContainer `SharedKernel` (`crates/thegn-core/src/capabilities.rs:240`). The matrix claims isolated filesystem and enforceable network for this reserved backend (`crates/thegn-core/src/sandbox_matrix.rs:223`, `:243`).

Repository-wide searches found no AppContainer security-token/process creation API implementation (`CreateAppContainer`, `DeriveAppContainer`, `SECURITY_CAPABILITIES`, or process-thread security-capability attribute). Windows process code implements kill-on-close Job Objects (`crates/thegn-host/src/platform/windows.rs:784`, `:817`, `:911`), which scope lifetime and do not deny filesystem/network access. Job creation/assignment is best-effort for general grouped spawning, with a separate fail-closed gate grouping path.

Impact: a reserved feature can satisfy a shared-kernel floor and display a containment label before its implementation exists. Treat native AppContainer as unavailable until token/ACL/network enforcement and launch failure handling are implemented and validated. This conclusion is source-based; Windows canaries were not executed. JobObject correctly ranks as `HostProcess` (`crates/thegn-core/src/capabilities.rs:237`).

### B08 — Medium: audit subscription does not match new container ownership labels

Affected: local Podman exec/network event audit.

OCI create sets `thegn.managed=true` (`crates/thegn-core/src/sandbox.rs:3150`, `crates/thegn-core/src/sandbox_manage.rs:39`), while both Podman subscriptions filter `label=io.thegn=true` (`crates/thegn-core/src/sandbox_events_podman.rs:32`). New containers created by this path lack that legacy label, so they do not match the subscription. Event selection chooses the first *events-capable* backend, not every actually launched backend (`crates/thegn-host/src/sandbox_events.rs:103`); stream stderr is discarded, and EOF ends subscription without reconnect (`crates/thegn-core/src/sandbox_events_podman.rs:79`, `:110`). Docker/Apple events are reserved in backend profiles (`crates/thegn-core/src/sandbox.rs:240`, `:270`).

`network_audit` starts a runtime event subscription (`crates/thegn-host/src/sandbox_events.rs:59`), not a packet/connection log. The DNS ring holds only 512 names/allow booleans, lacks sandbox identity, and is drainable only through callers; searches found production host code does not call `drain_events` (`crates/thegn-core/src/dns_filter.rs:21`, `:79`, `:101`).

Impact: UI audit rows are incomplete and are not evidence that commands or destination policies were enforced. Fix labels and subscribe per actual runtime/placement; persist structured connection-denial events, container policy identity, launch/fallback/refusal outcomes and reconciliation actions.

### B09 — Medium: block-only DNS matching is case-sensitive

Affected: hostname block-only policies whenever queries reach the interceptor.

`name_matches` trims a trailing dot but compares unnormalized strings (`crates/thegn-core/src/dns_filter.rs:55`). Reproduction verifies `blocked.test` is denied and `BLOCKED.TEST` is allowed under `allow=[]`, `block=["blocked.test"]`. The UDP extraction preserves label case (`:192`). This is additional to the stronger DNS/packet and singleton issues. Normalize DNS names and validate patterns; selective packet policy must still replace DNS filtering as the security boundary.

### B10 — Medium: systemd file_access is partial hardening rather than filesystem scope

Affected: Linux `systemd-run` backend.

The backend always enters the host filesystem, adds `PrivateTmp`, applies `ProtectSystem=yes` and `ProtectHome=read-only/tmpfs` only under read_only_root, then adds bind properties (`crates/thegn-core/src/sandbox.rs:2775`). It does not build a private filesystem root according to file_access. Thus `file_access=none/worktree` does not hide system paths, and `read_only_root` does not turn every host path read-only. The matrix correctly calls the filesystem isolation partial (`crates/thegn-core/src/sandbox_matrix.rs:225`). Remaining reads/writes are constrained by those properties and ambient user permissions; this audit did not verify privileged writes or claim privilege escalation. Refuse private-workspace/file-only requirements on this backend until full filesystem policy exists.

### B11 — High: VPN sidecar attachment does not establish a mandatory tunnel-only datapath

Affected: local OCI VPN sidecar/proxy attachments, including the sealed-tunnel profile; behavior depends on provider/image/configuration and is not runtime-certified here.

The worktree joins the sidecar network namespace in sidecar **and proxy** modes (`crates/thegn-core/src/sandbox.rs:3359`). Sidecar arguments use `run -d --name`, provider flags, environment and mounts, with no explicit network-denial or packet-policy arguments (`crates/thegn-svc/src/vpn/mod.rs:762`). Tailscale proxy mode enables a userspace daemon and SOCKS listener (`:420`); attach injects ALL_PROXY/HTTP_PROXY/HTTPS_PROXY environment variables, with no enforcement of their use (`crates/thegn-host/src/agent.rs:1134`, `crates/thegn-svc/src/vpn/mod.rs:81`). Tailscale all-traffic exit-node routing is optional (`crates/thegn-svc/src/vpn/mod.rs:395`); its default is empty (`crates/thegn-core/src/config_vpn.rs:159`). Readiness checks daemon/interface presence, not absence of a direct route (`crates/thegn-svc/src/vpn/mod.rs:438`, `:484`, `:527`). No Thegn-installed firewall/kill-switch or direct-route denial was found in this module.

Impact: a process can ignore proxy environment settings; attaching an overlay or proving readiness does not establish the profile's promised “only route is the tunnel.” With the sidecar's default network still present, Thegn has not denied direct egress. Particular external tunnel images/configs might install routes/firewall rules, but that is provider behavior requiring verification, not a general guarantee of these launchers. Namespace separation from the *host stack* is distinct from restricting the namespace's egress. A policy runtime must install a deny-first datapath, allow provider transport separately, and fail closed before readiness and after disconnect. Avoid treating optional proxy env as confinement.

A running sidecar is also reused solely on `sidecar_running` (`crates/thegn-svc/src/vpn/mod.rs:728`), so changed provider/credentials/routes/plan are not reconciled. No actual tunnel was started; no real VPN credential was accessed. Test the bypass with a synthetic localhost proxy/sidecar and controlled endpoints in a later runtime check.

## Requested controls and actual backend mechanisms

The following describes source mechanisms for **fresh** launches. B01 can invalidate OCI reuse. “Deny” means no IP namespace route; it does not revoke Unix service sockets, credentials, or host-process preparation. All rows require runtime validation on each supported OS, user-namespace configuration and backend version.

| Backend / platform | Filesystem and home mechanism | Network=none / selective destination rules | Process/resource/lifecycle limits | Verification caveat |
| --- | --- | --- | --- | --- |
| Podman rootless / Linux | Container root + rw live worktree/common Git + default ro ambient home, rw caches/carveouts; sealed hides ambient home | `--network none`; NAT default/host supported; only optional local DNS interceptor for names | keep-id, with rootless default namespace retry; readonly root, NNP, caps, pids, CPU/memory; persistent `run -d` + `exec` | Kernel shared unless verified alternate OCI runtime; packet allowlists absent |
| Podman rootful / Linux | Same binds, daemon launched through `sudo -n`; local UID:GID requested | Same network options/filter limits | Same OCI hardening; persistent container | Rootful daemon trust; all mounted rw host data remains exposed |
| Docker / Linux | Same OCI binds/home/cache policy; local UID:GID requested | Same network options/filter limits | Same OCI hardening and persistence | Event streaming reserved; kernel shared unless actual alternate runtime verified |
| Podman/Docker / macOS | Linux VM/container root; host toolchain whole-home injection suppressed by ABI gate; live worktree/Git/custom binds still apply | OCI none/host/default-NAT argv; DNS routing must be verified across VM | OCI limits inside VM; persistent container | Mac VM file shares are a distinct dependency; ABI gate does not make all user mounts safe |
| Apple / macOS | Separate Linux VM per container; no host toolchain substrate; live worktree/Git/custom binds; sealed source equality check | OCI network argv attempted; selective interception unverified and DNS-only | readonly root/caps, integer CPU rounded up; NNP and pids omitted with warnings | Backend-specific CLI; no verified labels/events; none/network behavior needs actual canaries |
| bwrap / Linux | Explicit mount namespace, ro `/usr,/bin,/lib,/lib64,/etc,/nix/store,/run/current-system` substrate, private tmp/proc/dev, default ro home/rw carveouts; sealed tmpfs home | `--unshare-net` only for none; NAT/host share host network; no hostname enforcement | PID namespace; cap options; die-with-parent except daemon-persistent; no native pids limit; optional cgroup/nice wrapper | Host/user namespace support required; all `/etc` remains readable substrate |
| systemd-run / Linux | Ambient host root with partial ProtectSystem/ProtectHome/PrivateTmp and bind properties | PrivateNetwork only for none; NAT/host share host network; no hostname enforcement | NNP/TasksMax/cgroup CPU/memory; user service; no independent workspace storage | User-manager permissions must support requested namespaces; not a worktree-only root |
| Smol / Linux or macOS candidate | Docker-like argv reservation | Docker-like options; no verified backend enforcement | Docker-like options | Explicitly unverified (`crates/thegn-core/src/sandbox.rs:348`); refuse secure guarantees |
| WSL / Windows candidate | Aspirational `wsl.exe --` wrapping (only entry argv is visibly wrapped) | No verified confinement/network contract | Docker-like option reservation | Explicitly unverified; sealed home view refuses WSL; refuse secure guarantees |
| JobObject / Windows | Ambient host files/home | Host network; no denial/selective grant | kill-on-close process grouping; resource limits deferred | HostProcess; assignment may degrade for ordinary grouped process spawning |
| AppContainer / Windows candidate | Plain shell and reserved capability declaration | No implemented token/network boundary found | Existing general JobObject grouping only | B07: not available as a verified security runtime |
| host/none / all | Ambient host files/home | Host network; no policy | Optional resource caps/lifetime grouping | No security boundary |
| remote/provider placement | Transport/projected workspace plus selected remote backend, or bare shell | Local DNS interception skipped; provider policy external | Provider lifecycle and remote runtime | Provider class bypasses ranked floor; provider behavior requires separate verification |

Backend/platform eligibility is centralized in `crates/thegn-core/src/sandbox_backend.rs:240`: bwrap/systemd Linux only, Apple macOS only, WSL/native Windows Windows only; native Windows declines ordinary OCI local mounts. Host-toolchain backends are local-only. Podman/Docker/macOS are VM-backed; this protects the host kernel but does not protect bind-mounted host files.

## Existing safety gates and credential/lifecycle residuals

- Every `ensure`/`enter_argv` uses `launchable`: named-volume admission, Compose restriction refusal, sealed mount filtering/home gate, canonical host mount sources and syntax validation (`crates/thegn-core/src/sandbox.rs:697`, `:2084`, `:2342`). Compose refuses requested hardening/network filters/runtime/limits/VPN instead of silently claiming enforcement (`:763`). These protections should remain central to any replacement runtime.
- Repo policies may only narrow trusted profile/network/floors; repo backend selection, fallback order, env passthrough, remote, VPN, personal-home and Compose grants are forbidden (`crates/thegn-core/src/config_resolve.rs:448`, `:516`). Network-list intersection and accumulated block lists are present (`:658`), but their actual runtime effect is limited by B01–B03.
- Local host-sourced env values use owner-only atomic files; OCI cleanup follows create; systemd can receive an ExecStopPost cleanup (`crates/thegn-core/src/sandbox.rs:2985`, `:2132`, `:2845`). Startup sweep removes stale files based on one-hour age (`:3069`); an app that never restarts can retain crash residue longer than an hour. The source comment's “at most one hour before next startup GC” must not be interpreted as an active timer.
- Secret classification compares the supplied value against current host env, not a sensitivity/provenance field (`crates/thegn-core/src/sandbox.rs:2912`). Nonmatching supplied secrets can remain argv values; remote placements without stdin forwarding retain inline delivery (`:2926`, `:3259`). Every `env_overrides` value is embedded in `sh -lc` script text (`:2394`), which is passed as argv. Launcher evidence must trace concrete override provenance and distinguish local environment-file protection from this residual.
- Sealed profiles drop default SSH_AUTH_SOCK passthrough and credential-home mounts (`crates/thegn-core/src/sandbox.rs:1249`); other named passthrough tokens still follow configured values. `nix_daemon=true` mounts an explicitly trusted host Unix service even under network=none (`:1280`). UNIX service capabilities must be audited separately from IP denial.
- OCI state is a keep-alive container, not a durable workspace copy (`crates/thegn-core/src/sandbox.rs:2081`). Worktree binds survive because they are host files; private tmpfs/root mutations do not. Recreating a stale container force-removes it even for daemon-persistent panes (`:2004`). Teardown/GC identifies managed containers; these lifecycles cannot serve as the durability owner for the proposed private repository.
- Bwrap die-with-parent is removed only for daemon-persistent panes, which delegate session reaping to daemon close/expiry/boot sweep (`crates/thegn-core/src/sandbox.rs:2727`). Windows ordinary JobObject creation has a spawn→assign window and assignment degradation (`crates/thegn-host/src/platform/windows.rs:817`); fail-closed gate grouping has suspended assignment but still grants no filesystem/network boundary (`:860`).

## Verification performed and limits

`python3 docs/audits/sandbox-security-2026-10-13/backend-probe.py` passed twice. It directly executes extracted production `mounts_match`, `judge_inspect`, `DnsPolicy`, matching, singleton and UDP-server logic, with the logging-only macro replacement disclosed above. Output is retained in `backend-probe-output.txt`. Extracted source SHA-256: `sandbox.rs` = `62b374a9fabd62236129bf800696bb3c08125edc4c0cd8df462119e33a718899`; `dns_filter.rs` = `dd5b3900349dbf5ed7b5f203672578ce18e8c116d1a050c287393cae885a6382`. The harness requires Python 3 and Rust 2024 edition support (audit used rustc 1.97.0); it creates/removes its build directory under the system temp directory. This is a focused regression reproduction, not a full build, runtime certification, or test of the proposed design.

No production source changed; no real credential was read as a canary; no external test request was sent. External policy/runtime documentation and the private-copy architecture are handled by the main report. Pending runtime checks include actual OCI policy transitions, DNS CLI acceptance/host-loopback reachability, private/home/system-path canaries, platform-specific containment, alternate DNS/direct IP endpoints and restart recovery. Unavailable backend rows must remain unverified rather than being marked passed by skipped upstream integration tests (e.g. `crates/thegn-core/tests/sandbox_network_policy.rs:15` gates Podman execution on explicit opt-in and otherwise returns).
