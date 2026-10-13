# External policy runtime fit

Checked 2026-10-13 using primary vendor/project documentation. These are
documentation findings, not installed-runtime results. Thegn source audited:
`38e09ded78652c6cf56b0e3a7b6a7c16d18dcd95`.

## OpenShell version and policy fit

The plan's `latest/reference/policy-schema` URL now returns a missing page. Its
replacement is [Policy Schema Reference](https://docs.nvidia.com/openshell/v0.1.3/how-it-works/policies/schema);
the current latest index identifies v0.1.3. Use version-pinned docs and release
artifacts for a later adapter. The development lifecycle page was also checked
against [v0.1.3 Manage Sandboxes](https://docs.nvidia.com/openshell/v0.1.3/how-it-works/sandboxes/overview).

OpenShell's schema separates filesystem/process startup policy from live
network rules. Filesystem paths default to inaccessible unless granted, with
workdir/default-policy details that require explicit configuration. Critically,
Landlock compatibility defaults to `best_effort`, which can run without the
requested filesystem rules; require `hard_requirement`. Inspected endpoint
rules default to `audit`, which allows violations; require `enforce`. Policy
validation rejects unknown fields/duplicate keys. Endpoints support host/IP,
port, resolved-address constraints and optional request inspection. Numeric
workload identities must be non-root. These controls match a private-workspace
adapter if Thegn emits explicit policy and refuses missing prerequisites.
[Schema](https://docs.nvidia.com/openshell/v0.1.3/how-it-works/policies/schema)

Outbound policy is documented as default-deny by destination and executable.
Provider attachments add network rules to the selected sandbox policy, so
checking only the base policy is insufficient. A gateway global policy
**replaces** sandbox policies and suppresses provider rules; it is not the
intersection of user global settings and repo restrictions requested by this
design. Thegn must calculate and verify its effective ceiling independently,
and prevent a later provider attachment or runtime-side approval from widening
it. A dedicated gateway or a fixed administrator ceiling avoids ambiguous
ownership; sandbox grants still need Thegn's user-owned review record.
[Sandbox Policies](https://docs.nvidia.com/openshell/v0.1.3/how-it-works/policies/overview)

Network identity covers executable ancestors as well as connecting binaries;
authorizing an agent can authorize its tools. Binary hashes are pinned when
first observed, so writable authorized paths require careful ownership.
Native TCP endpoints apply connection rules without request restrictions.
Wildcard DNS grants can leak data through labels, and shared endpoints can
reach other tenants. Do not interpret an allowed service as safe payloads.
The documented resolver returns placeholder A records and empty AAAA answers,
which is a compatibility consideration for clients caching DNS or requiring
IPv6. Qualify agent CLI, Git, package managers, TLS/mTLS and non-HTTP clients.
[Network Rules](https://docs.nvidia.com/openshell/v0.1.3/how-it-works/policies/network-rules)

The documented security layers mediate connections independently of proxy
environment variables and distinguish L4 access from inspected API actions.
TLS interception/credential rewriting and TLS-skip exceptions need explicit
policy decisions. Endpoint permission is an exfiltration channel; API
credentials should stay in a broker, with exact destination/request scope.
[Security Best Practices](https://docs.nvidia.com/openshell/v0.1.3/security/best-practices)

## Transfer and durability fit

Upload/download and stop/start operations are available. Uploads merge into
existing directories, overwrite matching entries and do not delete unrelated
files. Git ignore filtering is default; symlinks are preserved. Download sources
are restricted to the canonical workspace, including symlink escapes. These
are transport features, not baseline-aware sync or reviewable transactions.
Thegn must transfer to quarantine, validate manifests and stage candidate files
without directly overwriting the source checkout. Transfers need bounded size
and a source-generation check.

Stop/start retains sandbox configuration and driver-supported workspace data.
An owned driver operation lost during gateway crash can leave a record blocked
indefinitely without a resolving API. Recovery design must handle that state
without deleting the private workspace or silently creating a new baseline.
[Manage Sandboxes](https://docs.nvidia.com/openshell/v0.1.3/how-it-works/sandboxes/overview)

Persistence is driver-specific: custom Docker/Podman workdirs use the container
writable layer and lose edits on removal/replacement. Kubernetes has workspace
PVCs; MicroVM has a persistent overlay. Select durable independently owned
storage rather than assuming retention from a sandbox name. Docker/Podman
named volumes require resource admission. Host binds need explicitly weakened
driver admission and can bypass workspace isolation; avoid them for the secure
adapter. Verify allowed volume destinations preserve the private repository
across runtime recreation. Kubernetes requires a CNI that enforces
NetworkPolicy, otherwise policy can be bypassed. Thegn must verify that
deployment precondition, not infer it from a Kubernetes driver selection.
[Sandbox Runtimes](https://docs.nvidia.com/openshell/v0.1.3/how-it-works/sandboxes/runtimes)

## Platform and installation fit

Documented supported CLI platforms are Debian/Ubuntu Linux amd64/arm64 and
macOS Apple Silicon using Docker Desktop; Windows WSL2 is experimental.
Docker, rootless Podman, Kubernetes and MicroVM drivers are documented.
The workload boundary requires Landlock ABI 3+, usable seccomp notification/
FD injection, task-memory access and socket device binding, with active runtime
probes. NixOS and native Windows are not automatically qualified by this matrix.
Stable-release qualification covers part of the complete release policy;
Thegn still needs its own conformance suite and version pinning. Do not promise
cross-platform equivalence from the CLI being available.
[Support Matrix](https://docs.nvidia.com/openshell/v0.1.3/about/support-matrix)

## MXC status check

Current main revision obtained through the GitHub API:
`4cb972d696330b4559f930ff57ac4e5db65b5c80`. The
[README at that revision](https://github.com/microsoft/mxc/blob/4cb972d696330b4559f930ff57ac4e5db65b5c80/README.md)
no longer contains the plan's preview warning that profiles are not security
boundaries. It describes cross-platform containment and lifecycle SDKs;
network host filtering remains backend-dependent. Audit mode disables sandbox
security and cannot be used with untrusted workloads. The historical warning
is therefore not an accurate quote of current main. Its removal alone is not
runtime qualification or a security assurance for a packaged version.

The current
[Bubblewrap guide](https://github.com/microsoft/mxc/blob/4cb972d696330b4559f930ff57ac4e5db65b5c80/docs/backends/bwrap/bubblewrap-backend.md)
describes stable Linux support, private namespaces and deny defaults. Networked
modes need slirp4netns and namespace-local firewall tools; address rules require
IP literals/CIDRs, not DNS hostnames. Unsupported policy combinations refuse
validation. IPv6 grants are currently unreachable. Proxy-only enforcement is a
possible fit for Thegn's endpoint policies, but Thegn would still need to own
the proxy and durable workspace design. Also override default HOME behavior:
the guide describes using cwd as HOME, so untrusted project dotfiles could
become user configuration. The guide uses an alpha schema version despite
calling the backend stable; pin the actual SDK/schema and validate contracts.

**Decision:** MXC can be reconsidered in a separate qualification effort because
the explicitly named preview blocker has changed. Do not select it for this
milestone or assume every profile is a security boundary. Its renewed review
must include per-backend filesystem, packet policy, credential, persistence,
process and crash tests on supported platforms. OpenShell remains the primary
candidate in this comparison because its documented policy/credential runtime
matches the desired endpoint model; neither has been installed or empirically
qualified here.

## Adapter go/no-go criteria

Before committing to an external runtime, require a pinned stable artifact and
all of the following in a disposable, credential-free environment:

- Every untrusted launcher enters a verified policy generation; missing gateway,
  rejected policy, unsupported kernel and runtime loss never trigger host exec.
- Filesystem and API request rules actually enforce; static changes recreate
  compute while preserving private storage; effective provider rules remain
  within the user's ceiling.
- Direct-IP, alternate DNS, proxy-variable removal, UDP, raw socket, IPv6,
  mapped-address, host/LAN/metadata and cross-workspace probes cannot bypass
  denial. Logs identify the real workload/policy generation.
- Independent Git, dirty/untracked/binary transfer, interrupted export and
  quarantine checks work without shared host metadata or automatic host writes.
- App/gateway/runtime crashes and container replacement preserve authoritative
  private data; blocked lifecycle states have a documented recovery path.
- Credential delegation and revocation, PTY resize/detach/reattach, scoped
  localhost services, offline images and tool compatibility satisfy Thegn UX.

No vendor performance/cost/support recommendation is based on marketing alone.
These gates permit a later implementation choice while delivering a usable
native offline path independently.
