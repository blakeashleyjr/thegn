//! The pure **host provisioning state machine**: one event in, a new state and
//! a batch of effects out. Effects are plain data executed by an impure driver
//! (the service crate / host_flow) — this module does no I/O, so every branch
//! is unit-tested under the coverage gate (the `lifecycle::decide` pattern).
//!
//! ```text
//! Unknown → Connecting → Probing ─ runtime present ────────────→ (RuntimeReady*)
//!                          └ absent → AwaitingConsent → Installing → (RuntimeReady*)
//! → ImageResolving ─ digest present on host ─→ (ImageReady*)
//!                  └ absent → Delivering{strategy ladder} ─→ (ImageReady*)
//! → VolumeSeeding → Ready*          (any) → Failed{step, error, retryable}*
//! ```
//!
//! `*` = durable checkpoint: the only states ever persisted (via the
//! [`HostEffect::Checkpoint`] effect), so a killed driver resumes from the
//! nearest checkpoint by construction. Transient states re-enter through
//! `Connecting` on resume; the on-host work (probe, `image exists`, idempotent
//! volume seed) makes the replay cheap.

use crate::host::{HostCaps, HostFailure, HostStep, RuntimeInfo, RuntimeKind, VolumeSpec};
use crate::host_config::InstallConsent;
use crate::image::{DeliveryStrategy, Digest, ImageRef, LocalCaps, ResolvedImage, select_delivery};

/// The machine's states. Only [`HostState::durable_tag`] states are persisted.
#[derive(Debug, Clone, PartialEq)]
pub enum HostState {
    Unknown,
    Connecting,
    Probing,
    /// Runtime absent, consent = `ask`: parked until the driver resolves the
    /// user's answer (modal / CLI prompt / panel action).
    AwaitingConsent {
        runtime: RuntimeKind,
    },
    Installing {
        runtime: RuntimeKind,
    },
    /// Durable: a runtime is confirmed on the host.
    RuntimeReady,
    ImageResolving,
    Delivering {
        strategy: DeliveryStrategy,
    },
    /// Durable: the per-arch base image digest is verified on the host.
    ImageReady,
    VolumeSeeding {
        remaining: Vec<VolumeSpec>,
    },
    /// Durable: everything a sandbox spawn needs is on the host.
    Ready,
    /// Durable: what died, why, and whether a plain retry can succeed.
    Failed(HostFailure),
}

impl HostState {
    /// The persisted tag for durable states; `None` for transients (which are
    /// never written).
    pub fn durable_tag(&self) -> Option<&'static str> {
        match self {
            HostState::Unknown => Some("unknown"),
            HostState::RuntimeReady => Some("runtime_ready"),
            HostState::ImageReady => Some("image_ready"),
            HostState::Ready => Some("ready"),
            HostState::Failed(_) => Some("failed"),
            _ => None,
        }
    }

    /// Rebuild a durable state from its tag (+ failure meta for `failed`).
    /// Unknown tags land on `Unknown` — a full (idempotent) re-provision.
    pub fn from_durable_tag(tag: &str, failure: Option<HostFailure>) -> HostState {
        match tag {
            "runtime_ready" => HostState::RuntimeReady,
            "image_ready" => HostState::ImageReady,
            "ready" => HostState::Ready,
            "failed" => HostState::Failed(failure.unwrap_or(HostFailure {
                step: HostStep::Connect,
                error: "unknown failure".into(),
                retryable: true,
            })),
            _ => HostState::Unknown,
        }
    }

    /// The step a state is "at" — used to tag failures on illegal events.
    fn step(&self) -> HostStep {
        match self {
            HostState::Unknown | HostState::Connecting => HostStep::Connect,
            HostState::Probing => HostStep::Probe,
            HostState::AwaitingConsent { .. } => HostStep::Consent,
            HostState::Installing { .. } => HostStep::Install,
            HostState::RuntimeReady | HostState::ImageResolving => HostStep::ResolveImage,
            HostState::Delivering { .. } => HostStep::Deliver,
            HostState::ImageReady | HostState::VolumeSeeding { .. } => HostStep::SeedVolume,
            HostState::Ready | HostState::Failed(_) => HostStep::Verify,
        }
    }
}

/// Results of executed effects, reported back by the driver.
#[derive(Debug, Clone)]
pub enum HostEvent {
    /// Kick / resume the machine.
    Start,
    Connected,
    ConnectFailed {
        error: String,
    },
    Probed(HostCaps),
    ProbeFailed {
        error: String,
    },
    ConsentGranted,
    ConsentDenied,
    Installed(RuntimeInfo),
    InstallFailed {
        error: String,
    },
    /// The image reference resolved to per-arch digests.
    ImageResolved(ResolvedImage),
    ResolveFailed {
        error: String,
    },
    /// On-host digest check result for the wanted per-arch digest.
    ImagePresent,
    ImageAbsent,
    /// A delivery strategy finished and the on-host digest was verified.
    Delivered {
        verified_digest: Digest,
    },
    /// The CURRENT strategy is exhausted (the driver does bounded in-strategy
    /// retries first); the machine falls down the ladder.
    DeliverFailed {
        error: String,
    },
    VolumeSeeded {
        volume: String,
    },
    VolumeSeedFailed {
        volume: String,
        error: String,
    },
}

/// What the impure driver must do next. Data only — no closures, no channels.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEffect {
    /// Open (or re-open) the control channel for the host's reach.
    Connect,
    /// Run the single-shot probe script; report `Probed`/`ProbeFailed`.
    Probe,
    /// Surface the install-consent question; report `ConsentGranted/Denied`.
    AskConsent { runtime: RuntimeKind },
    /// Bootstrap the runtime (consent already granted by construction).
    Install { runtime: RuntimeKind },
    /// Resolve the image reference to per-arch digests (manifest inspect).
    ResolveImage { reference: ImageRef },
    /// `image exists <name@digest>` on the host; report Present/Absent.
    CheckImage { digest: Digest },
    /// Execute one delivery strategy for `digest`; report Delivered/Failed.
    Deliver {
        strategy: DeliveryStrategy,
        digest: Digest,
    },
    /// Idempotently seed one warm volume (exists ⇒ no-op success).
    SeedVolume { spec: VolumeSpec },
    /// Persist a durable checkpoint (hosts row + inventory as applicable).
    Checkpoint { state: HostState },
    /// Progress line for the event trail + UI.
    Emit { step: HostStep, detail: String },
}

/// Immutable inputs + accumulated knowledge the machine consults. The driver
/// owns it; [`step`] fills `caps`/`resolved`/`strategies` as events land.
#[derive(Debug, Clone)]
pub struct MachineCtx {
    pub consent: InstallConsent,
    pub local: LocalCaps,
    pub wanted_image: ImageRef,
    pub wanted_volumes: Vec<VolumeSpec>,
    pub delivery_prefs: Vec<crate::host::DeliveryCap>,
    /// Filled by `Probed` (or synthesized for cloud reaches before `Start`).
    pub caps: Option<HostCaps>,
    /// Filled by `ImageResolved`; pre-fill from persisted inventory on resume
    /// to skip the (network) resolve step entirely.
    pub resolved: Option<ResolvedImage>,
    /// Ranked delivery ladder, filled after `Probed`.
    pub strategies: Vec<DeliveryStrategy>,
}

impl MachineCtx {
    pub fn new(
        consent: InstallConsent,
        local: LocalCaps,
        wanted_image: ImageRef,
        wanted_volumes: Vec<VolumeSpec>,
        delivery_prefs: Vec<crate::host::DeliveryCap>,
    ) -> MachineCtx {
        MachineCtx {
            consent,
            local,
            wanted_image,
            wanted_volumes,
            delivery_prefs,
            caps: None,
            resolved: None,
            strategies: Vec::new(),
        }
    }

    /// The per-arch digest the host must hold (needs `caps` + `resolved`).
    fn wanted_digest(&self) -> Option<Digest> {
        let arch = self.caps.as_ref()?.arch;
        self.resolved.as_ref()?.digest_for(arch).cloned()
    }
}

/// One transition: the next state plus the effects the driver must execute.
#[derive(Debug, Clone)]
pub struct Transition {
    pub next: HostState,
    pub effects: Vec<HostEffect>,
}

fn fail(step: HostStep, error: impl Into<String>, retryable: bool) -> Transition {
    let failure = HostFailure {
        step,
        error: error.into(),
        retryable,
    };
    let state = HostState::Failed(failure);
    Transition {
        effects: vec![HostEffect::Checkpoint {
            state: state.clone(),
        }],
        next: state,
    }
}

fn emit(step: HostStep, detail: impl Into<String>) -> HostEffect {
    HostEffect::Emit {
        step,
        detail: detail.into(),
    }
}

/// After the image is confirmed on the host: checkpoint `ImageReady` and enter
/// volume seeding (or go straight to `Ready` when nothing to seed).
fn enter_volumes(ctx: &MachineCtx) -> Transition {
    let mut effects = vec![HostEffect::Checkpoint {
        state: HostState::ImageReady,
    }];
    let remaining = ctx.wanted_volumes.clone();
    match remaining.first() {
        None => {
            effects.push(HostEffect::Checkpoint {
                state: HostState::Ready,
            });
            effects.push(emit(HostStep::Verify, "ready"));
            Transition {
                next: HostState::Ready,
                effects,
            }
        }
        Some(first) => {
            effects.push(emit(HostStep::SeedVolume, format!("seed {}", first.name)));
            effects.push(HostEffect::SeedVolume {
                spec: first.clone(),
            });
            Transition {
                next: HostState::VolumeSeeding { remaining },
                effects,
            }
        }
    }
}

/// After a runtime is confirmed: checkpoint `RuntimeReady` and enter image
/// resolution — skipping the (network) resolve when `ctx.resolved` was
/// pre-filled from inventory, and skipping the on-host check entirely is never
/// done: boot always digest-verifies.
fn enter_image(ctx: &MachineCtx) -> Transition {
    let mut effects = vec![HostEffect::Checkpoint {
        state: HostState::RuntimeReady,
    }];
    match ctx.wanted_digest() {
        Some(digest) => {
            effects.push(emit(HostStep::Verify, format!("check {}", digest.short())));
            effects.push(HostEffect::CheckImage { digest });
        }
        None => {
            effects.push(emit(
                HostStep::ResolveImage,
                format!("resolve {}", ctx.wanted_image),
            ));
            effects.push(HostEffect::ResolveImage {
                reference: ctx.wanted_image.clone(),
            });
        }
    }
    Transition {
        next: HostState::ImageResolving,
        effects,
    }
}

/// THE pure transition function. Total over (state, event): pairs the machine
/// doesn't expect land on `Failed { retryable: true }` (a driver bug or a
/// stale event must never wedge a host forever).
pub fn step(state: &HostState, ctx: &mut MachineCtx, ev: HostEvent) -> Transition {
    match (state, ev) {
        // ── connect ────────────────────────────────────────────────────────
        (HostState::Unknown, HostEvent::Start) | (HostState::Connecting, HostEvent::Start) => {
            Transition {
                next: HostState::Connecting,
                effects: vec![emit(HostStep::Connect, "connect"), HostEffect::Connect],
            }
        }
        (HostState::Connecting, HostEvent::Connected) => Transition {
            next: HostState::Probing,
            effects: vec![emit(HostStep::Probe, "probe"), HostEffect::Probe],
        },
        (HostState::Connecting, HostEvent::ConnectFailed { error }) => {
            fail(HostStep::Connect, error, true)
        }

        // ── probe ──────────────────────────────────────────────────────────
        (HostState::Probing, HostEvent::Probed(caps)) => {
            ctx.strategies = select_delivery(&ctx.local, &caps, &ctx.delivery_prefs);
            let runtime = caps.runtime.clone();
            let can_install = caps.can_install_runtime;
            ctx.caps = Some(caps);
            match runtime {
                Some(rt) => {
                    let mut t = enter_image(ctx);
                    t.effects.insert(
                        0,
                        emit(
                            HostStep::Probe,
                            format!("{} {}", rt.kind.as_str(), rt.version),
                        ),
                    );
                    t
                }
                None => match ctx.consent {
                    InstallConsent::Auto if can_install => Transition {
                        next: HostState::Installing {
                            runtime: RuntimeKind::Podman,
                        },
                        effects: vec![
                            emit(HostStep::Install, "install podman (pre-granted)"),
                            HostEffect::Install {
                                runtime: RuntimeKind::Podman,
                            },
                        ],
                    },
                    InstallConsent::Ask if can_install => Transition {
                        next: HostState::AwaitingConsent {
                            runtime: RuntimeKind::Podman,
                        },
                        effects: vec![
                            emit(HostStep::Consent, "awaiting install consent"),
                            HostEffect::AskConsent {
                                runtime: RuntimeKind::Podman,
                            },
                        ],
                    },
                    InstallConsent::Never => fail(
                        HostStep::Install,
                        "no container runtime and install is disallowed \
                         (install_runtime = \"never\")",
                        false,
                    ),
                    _ => fail(
                        HostStep::Install,
                        "no container runtime and no supported package manager to install one",
                        false,
                    ),
                },
            }
        }
        (HostState::Probing, HostEvent::ProbeFailed { error }) => {
            fail(HostStep::Probe, error, true)
        }

        // ── consent + install ──────────────────────────────────────────────
        (HostState::AwaitingConsent { runtime }, HostEvent::ConsentGranted) => Transition {
            next: HostState::Installing { runtime: *runtime },
            effects: vec![
                emit(HostStep::Install, format!("install {}", runtime.as_str())),
                HostEffect::Install { runtime: *runtime },
            ],
        },
        (HostState::AwaitingConsent { .. }, HostEvent::ConsentDenied) => fail(
            HostStep::Consent,
            "install declined — grant from the Hosts panel or set install_runtime = \"auto\"",
            false,
        ),
        (HostState::Installing { .. }, HostEvent::Installed(rt)) => {
            if let Some(caps) = ctx.caps.as_mut() {
                caps.runtime = Some(rt.clone());
            }
            let mut t = enter_image(ctx);
            t.effects.insert(
                0,
                emit(
                    HostStep::Install,
                    format!("installed {} {}", rt.kind.as_str(), rt.version),
                ),
            );
            t
        }
        (HostState::Installing { .. }, HostEvent::InstallFailed { error }) => {
            fail(HostStep::Install, error, true)
        }

        // ── image resolve + presence ───────────────────────────────────────
        (HostState::ImageResolving, HostEvent::ImageResolved(resolved)) => {
            ctx.resolved = Some(resolved);
            match ctx.wanted_digest() {
                Some(digest) => Transition {
                    next: HostState::ImageResolving,
                    effects: vec![
                        emit(HostStep::Verify, format!("check {}", digest.short())),
                        HostEffect::CheckImage { digest },
                    ],
                },
                None => {
                    let arch = ctx
                        .caps
                        .as_ref()
                        .map(|c| c.arch.oci_name())
                        .unwrap_or("unknown");
                    fail(
                        HostStep::ResolveImage,
                        format!("{} has no {arch} manifest", ctx.wanted_image),
                        false,
                    )
                }
            }
        }
        (HostState::ImageResolving, HostEvent::ResolveFailed { error }) => {
            fail(HostStep::ResolveImage, error, true)
        }
        (HostState::ImageResolving, HostEvent::ImagePresent) => {
            let mut t = enter_volumes(ctx);
            t.effects.insert(0, emit(HostStep::Verify, "image present"));
            t
        }
        (HostState::ImageResolving, HostEvent::ImageAbsent) => {
            let Some(digest) = ctx.wanted_digest() else {
                return fail(
                    HostStep::Deliver,
                    "image absent but no resolved digest (driver bug)",
                    true,
                );
            };
            match ctx.strategies.first() {
                Some(strategy) => Transition {
                    next: HostState::Delivering {
                        strategy: *strategy,
                    },
                    effects: vec![
                        emit(
                            HostStep::Deliver,
                            format!("deliver via {}", strategy.as_str()),
                        ),
                        HostEffect::Deliver {
                            strategy: *strategy,
                            digest,
                        },
                    ],
                },
                None => fail(
                    HostStep::Deliver,
                    "no delivery route available (no local image source, no host egress)",
                    true,
                ),
            }
        }

        // ── delivery ladder ────────────────────────────────────────────────
        (HostState::Delivering { .. }, HostEvent::Delivered { verified_digest }) => {
            match ctx.wanted_digest() {
                Some(want) if want == verified_digest => {
                    let mut t = enter_volumes(ctx);
                    t.effects.insert(
                        0,
                        emit(HostStep::Deliver, format!("delivered {}", want.short())),
                    );
                    t
                }
                Some(want) => fail(
                    HostStep::Verify,
                    format!(
                        "digest mismatch after delivery (want {}, got {}) — refusing to boot",
                        want.short(),
                        verified_digest.short()
                    ),
                    true,
                ),
                None => fail(
                    HostStep::Verify,
                    "delivered without a resolved digest (driver bug)",
                    true,
                ),
            }
        }
        (HostState::Delivering { strategy }, HostEvent::DeliverFailed { error }) => {
            match next_strategy(ctx, strategy) {
                Some(next) => {
                    let Some(digest) = ctx.wanted_digest() else {
                        return fail(HostStep::Deliver, error, true);
                    };
                    Transition {
                        next: HostState::Delivering { strategy: next },
                        effects: vec![
                            emit(
                                HostStep::Deliver,
                                format!(
                                    "{} failed ({error}) — falling back to {}",
                                    strategy.as_str(),
                                    next.as_str()
                                ),
                            ),
                            HostEffect::Deliver {
                                strategy: next,
                                digest,
                            },
                        ],
                    }
                }
                None => fail(
                    HostStep::Deliver,
                    format!("all delivery routes failed (last: {error})"),
                    true,
                ),
            }
        }

        // ── volumes ────────────────────────────────────────────────────────
        (HostState::VolumeSeeding { remaining }, HostEvent::VolumeSeeded { volume }) => {
            let rest: Vec<VolumeSpec> = remaining
                .iter()
                .filter(|v| v.name != volume)
                .cloned()
                .collect();
            match rest.first() {
                None => Transition {
                    next: HostState::Ready,
                    effects: vec![
                        HostEffect::Checkpoint {
                            state: HostState::Ready,
                        },
                        emit(HostStep::Verify, "ready"),
                    ],
                },
                Some(next) => Transition {
                    effects: vec![
                        emit(HostStep::SeedVolume, format!("seed {}", next.name)),
                        HostEffect::SeedVolume { spec: next.clone() },
                    ],
                    next: HostState::VolumeSeeding { remaining: rest },
                },
            }
        }
        (HostState::VolumeSeeding { .. }, HostEvent::VolumeSeedFailed { volume, error }) => fail(
            HostStep::SeedVolume,
            format!("seeding {volume}: {error}"),
            true,
        ),

        // ── terminal states ────────────────────────────────────────────────
        (HostState::Ready, HostEvent::Start) => Transition {
            next: HostState::Ready,
            effects: vec![],
        },
        (HostState::Failed(f), HostEvent::Start) if f.retryable => Transition {
            next: HostState::Connecting,
            effects: vec![
                emit(HostStep::Connect, format!("retry after: {}", f.error)),
                HostEffect::Connect,
            ],
        },
        (HostState::Failed(f), HostEvent::Start) => Transition {
            next: HostState::Failed(f.clone()),
            effects: vec![],
        },

        // ── totality: an unexpected pair must never wedge the driver ───────
        (state, ev) => fail(
            state.step(),
            format!("unexpected event {ev:?} in state {state:?}"),
            true,
        ),
    }
}

/// Resume from a persisted durable state. A fresh `Ready` (probe within
/// `probe_ttl_secs`) is a no-op; anything else re-enters through `Connecting`
/// — the on-host replay (probe + digest check + idempotent seeds) is cheap and
/// each durable checkpoint's work is skipped by construction. A non-retryable
/// `Failed` stays failed until an explicit user reset.
pub fn resume(
    persisted: HostState,
    last_probe: Option<i64>,
    now: i64,
    probe_ttl_secs: i64,
) -> (HostState, Vec<HostEffect>) {
    match persisted {
        HostState::Ready => {
            let fresh = last_probe.is_some_and(|t| now.saturating_sub(t) <= probe_ttl_secs);
            if fresh {
                (HostState::Ready, vec![])
            } else {
                (
                    HostState::Connecting,
                    vec![
                        emit(HostStep::Probe, "probe ttl lapsed — re-verify"),
                        HostEffect::Connect,
                    ],
                )
            }
        }
        HostState::Failed(f) if !f.retryable => (HostState::Failed(f), vec![]),
        // Every other durable state (Unknown / RuntimeReady / ImageReady /
        // retryable Failed) — and, defensively, any transient that leaked —
        // re-enters the machine from the top.
        _ => (
            HostState::Connecting,
            vec![emit(HostStep::Connect, "resume"), HostEffect::Connect],
        ),
    }
}

/// The delivery fallback ladder: the ranked strategy after `failed`, if any.
pub fn next_strategy(ctx: &MachineCtx, failed: &DeliveryStrategy) -> Option<DeliveryStrategy> {
    let i = ctx.strategies.iter().position(|s| s == failed)?;
    ctx.strategies.get(i + 1).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Arch, VolumeSeed};

    const D_AMD: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const D_ARM: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const D_LIST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn caps(runtime: bool, pkgmgr: bool) -> HostCaps {
        let mut probe = String::from("ARCH=x86_64\nOS=linux\nRSYNC=1\n");
        if runtime {
            probe.push_str("PODMAN=5.0\nPODMAN_ROOTLESS=1\n");
        }
        if pkgmgr {
            probe.push_str("PKGMGR=apt\n");
        }
        HostCaps::parse_probe(&probe).unwrap()
    }

    fn resolved() -> ResolvedImage {
        ResolvedImage {
            reference: ImageRef::parse("ghcr.io/x/base:v1").unwrap(),
            list_digest: Digest::parse(D_LIST).unwrap(),
            per_arch: [
                (Arch::Amd64, Digest::parse(D_AMD).unwrap()),
                (Arch::Arm64, Digest::parse(D_ARM).unwrap()),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn vols() -> Vec<VolumeSpec> {
        vec![
            VolumeSpec::by_name("nix-store").unwrap(),
            VolumeSpec::by_name("cargo").unwrap(),
        ]
    }

    fn ctx(consent: InstallConsent, volumes: Vec<VolumeSpec>) -> MachineCtx {
        MachineCtx::new(
            consent,
            LocalCaps {
                has_podman: true,
                has_skopeo: false,
                has_rsync: true,
                has_registry_egress: true,
            },
            ImageRef::parse("ghcr.io/x/base:v1").unwrap(),
            volumes,
            Vec::new(),
        )
    }

    fn has_effect(t: &Transition, pred: impl Fn(&HostEffect) -> bool) -> bool {
        t.effects.iter().any(pred)
    }

    fn checkpoints(t: &Transition) -> Vec<&'static str> {
        t.effects
            .iter()
            .filter_map(|e| match e {
                HostEffect::Checkpoint { state } => state.durable_tag(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn happy_path_full_walk() {
        let mut c = ctx(InstallConsent::Ask, vols());
        let t = step(&HostState::Unknown, &mut c, HostEvent::Start);
        assert_eq!(t.next, HostState::Connecting);
        assert!(has_effect(&t, |e| *e == HostEffect::Connect));

        let t = step(&t.next, &mut c, HostEvent::Connected);
        assert_eq!(t.next, HostState::Probing);
        assert!(has_effect(&t, |e| *e == HostEffect::Probe));

        // Runtime present ⇒ RuntimeReady checkpoint + resolve (no cached digest).
        let t = step(&t.next, &mut c, HostEvent::Probed(caps(true, true)));
        assert_eq!(t.next, HostState::ImageResolving);
        assert_eq!(checkpoints(&t), vec!["runtime_ready"]);
        assert!(has_effect(&t, |e| matches!(
            e,
            HostEffect::ResolveImage { .. }
        )));

        let t = step(&t.next, &mut c, HostEvent::ImageResolved(resolved()));
        assert_eq!(t.next, HostState::ImageResolving);
        assert!(has_effect(
            &t,
            |e| matches!(e, HostEffect::CheckImage { digest } if digest.as_str() == D_AMD)
        ));

        // Absent ⇒ deliver via the default (registry-less) ladder head.
        let t = step(&t.next, &mut c, HostEvent::ImageAbsent);
        let HostState::Delivering { strategy } = t.next.clone() else {
            panic!("expected Delivering, got {:?}", t.next);
        };
        assert_eq!(strategy, DeliveryStrategy::SshStream { rsync: true });

        let t = step(
            &t.next,
            &mut c,
            HostEvent::Delivered {
                verified_digest: Digest::parse(D_AMD).unwrap(),
            },
        );
        assert_eq!(checkpoints(&t), vec!["image_ready"]);
        let HostState::VolumeSeeding { remaining } = t.next.clone() else {
            panic!("expected VolumeSeeding, got {:?}", t.next);
        };
        assert_eq!(remaining.len(), 2);

        let t = step(
            &t.next,
            &mut c,
            HostEvent::VolumeSeeded {
                volume: crate::host::VOLUME_NIX_STORE.into(),
            },
        );
        assert!(matches!(&t.next, HostState::VolumeSeeding { remaining } if remaining.len() == 1));
        assert!(has_effect(&t, |e| matches!(
            e,
            HostEffect::SeedVolume { spec } if spec.name == crate::host::VOLUME_CARGO
        )));

        let t = step(
            &t.next,
            &mut c,
            HostEvent::VolumeSeeded {
                volume: crate::host::VOLUME_CARGO.into(),
            },
        );
        assert_eq!(t.next, HostState::Ready);
        assert_eq!(checkpoints(&t), vec!["ready"]);
    }

    #[test]
    fn image_present_skips_delivery_and_no_volumes_goes_straight_ready() {
        let mut c = ctx(InstallConsent::Ask, Vec::new());
        c.caps = Some(caps(true, true));
        c.resolved = Some(resolved());
        let t = step(&HostState::ImageResolving, &mut c, HostEvent::ImagePresent);
        assert_eq!(t.next, HostState::Ready);
        assert_eq!(checkpoints(&t), vec!["image_ready", "ready"]);
    }

    #[test]
    fn cached_resolution_skips_resolve_effect() {
        let mut c = ctx(InstallConsent::Ask, vols());
        c.resolved = Some(resolved());
        let t = step(
            &HostState::Probing,
            &mut c,
            HostEvent::Probed(caps(true, true)),
        );
        assert_eq!(t.next, HostState::ImageResolving);
        assert!(!has_effect(&t, |e| matches!(
            e,
            HostEffect::ResolveImage { .. }
        )));
        assert!(has_effect(&t, |e| matches!(
            e,
            HostEffect::CheckImage { .. }
        )));
    }

    #[test]
    fn consent_ladder() {
        // ask + installable ⇒ park on AwaitingConsent.
        let mut c = ctx(InstallConsent::Ask, vols());
        let t = step(
            &HostState::Probing,
            &mut c,
            HostEvent::Probed(caps(false, true)),
        );
        assert!(matches!(t.next, HostState::AwaitingConsent { .. }));
        assert!(has_effect(&t, |e| matches!(
            e,
            HostEffect::AskConsent { .. }
        )));

        // granted ⇒ install; denied ⇒ fatal.
        let granted = step(&t.next, &mut c, HostEvent::ConsentGranted);
        assert!(matches!(granted.next, HostState::Installing { .. }));
        let denied = step(&t.next, &mut c, HostEvent::ConsentDenied);
        let HostState::Failed(f) = denied.next else {
            panic!()
        };
        assert_eq!(f.step, HostStep::Consent);
        assert!(!f.retryable);

        // auto ⇒ straight to Installing.
        let mut c = ctx(InstallConsent::Auto, vols());
        let t = step(
            &HostState::Probing,
            &mut c,
            HostEvent::Probed(caps(false, true)),
        );
        assert!(matches!(t.next, HostState::Installing { .. }));

        // never ⇒ fatal with the config remedy in the message.
        let mut c = ctx(InstallConsent::Never, vols());
        let t = step(
            &HostState::Probing,
            &mut c,
            HostEvent::Probed(caps(false, true)),
        );
        let HostState::Failed(f) = t.next else {
            panic!()
        };
        assert!(!f.retryable);
        assert!(f.error.contains("never"));

        // ask but no package manager ⇒ fatal (nothing to consent to).
        let mut c = ctx(InstallConsent::Ask, vols());
        let t = step(
            &HostState::Probing,
            &mut c,
            HostEvent::Probed(caps(false, false)),
        );
        let HostState::Failed(f) = t.next else {
            panic!()
        };
        assert!(!f.retryable);
        assert!(f.error.contains("package manager"));
    }

    #[test]
    fn install_success_enters_image_and_failure_is_retryable() {
        let mut c = ctx(InstallConsent::Auto, vols());
        c.caps = Some(caps(false, true));
        let installing = HostState::Installing {
            runtime: RuntimeKind::Podman,
        };
        let rt = RuntimeInfo {
            kind: RuntimeKind::Podman,
            version: "5.1".into(),
            rootless: true,
            socket: None,
        };
        let t = step(&installing, &mut c, HostEvent::Installed(rt.clone()));
        assert_eq!(t.next, HostState::ImageResolving);
        assert_eq!(checkpoints(&t), vec!["runtime_ready"]);
        assert_eq!(c.caps.as_ref().unwrap().runtime.as_ref().unwrap(), &rt);

        let t = step(
            &installing,
            &mut c,
            HostEvent::InstallFailed {
                error: "apt broke".into(),
            },
        );
        let HostState::Failed(f) = t.next else {
            panic!()
        };
        assert!(f.retryable);
    }

    #[test]
    fn delivery_ladder_falls_back_then_exhausts() {
        let mut c = ctx(InstallConsent::Ask, vols());
        c.caps = Some(caps(true, true));
        c.resolved = Some(resolved());
        c.strategies = select_delivery(&c.local, c.caps.as_ref().unwrap(), &[]);
        assert!(c.strategies.len() >= 3, "{:?}", c.strategies);

        let first = c.strategies[0];
        let t = step(
            &HostState::Delivering { strategy: first },
            &mut c,
            HostEvent::DeliverFailed {
                error: "stalled".into(),
            },
        );
        let HostState::Delivering { strategy: second } = t.next.clone() else {
            panic!("expected fallback, got {:?}", t.next)
        };
        assert_eq!(second, c.strategies[1]);
        assert!(has_effect(&t, |e| matches!(e, HostEffect::Deliver { .. })));

        // Exhaust the whole ladder.
        let last = *c.strategies.last().unwrap();
        let t = step(
            &HostState::Delivering { strategy: last },
            &mut c,
            HostEvent::DeliverFailed {
                error: "nope".into(),
            },
        );
        let HostState::Failed(f) = t.next else {
            panic!()
        };
        assert!(f.retryable);
        assert!(f.error.contains("all delivery routes failed"));
    }

    #[test]
    fn digest_mismatch_refuses_to_boot() {
        let mut c = ctx(InstallConsent::Ask, vols());
        c.caps = Some(caps(true, true));
        c.resolved = Some(resolved());
        let t = step(
            &HostState::Delivering {
                strategy: DeliveryStrategy::RegistryPull,
            },
            &mut c,
            HostEvent::Delivered {
                verified_digest: Digest::parse(D_ARM).unwrap(), // wrong arch digest
            },
        );
        let HostState::Failed(f) = t.next else {
            panic!()
        };
        assert_eq!(f.step, HostStep::Verify);
        assert!(f.error.contains("refusing to boot"));
    }

    #[test]
    fn missing_arch_manifest_is_fatal() {
        let mut c = ctx(InstallConsent::Ask, vols());
        let mut probe = caps(true, true);
        probe.arch = Arch::Arm64;
        c.caps = Some(probe);
        let mut r = resolved();
        r.per_arch.remove(&Arch::Arm64);
        let t = step(
            &HostState::ImageResolving,
            &mut c,
            HostEvent::ImageResolved(r),
        );
        let HostState::Failed(f) = t.next else {
            panic!()
        };
        assert!(!f.retryable);
        assert!(f.error.contains("arm64"));
    }

    #[test]
    fn no_delivery_route_is_retryable() {
        let mut c = ctx(InstallConsent::Ask, vols());
        c.caps = Some(caps(true, true));
        c.resolved = Some(resolved());
        c.strategies = Vec::new();
        let t = step(&HostState::ImageResolving, &mut c, HostEvent::ImageAbsent);
        let HostState::Failed(f) = t.next else {
            panic!()
        };
        assert!(f.retryable);
        assert!(f.error.contains("no delivery route"));
    }

    #[test]
    fn volume_seed_failure_is_retryable() {
        let mut c = ctx(InstallConsent::Ask, vols());
        let t = step(
            &HostState::VolumeSeeding { remaining: vols() },
            &mut c,
            HostEvent::VolumeSeedFailed {
                volume: "superzej-nix-store".into(),
                error: "disk full".into(),
            },
        );
        let HostState::Failed(f) = t.next else {
            panic!()
        };
        assert_eq!(f.step, HostStep::SeedVolume);
        assert!(f.retryable);
    }

    #[test]
    fn terminal_states_handle_start() {
        let mut c = ctx(InstallConsent::Ask, vols());
        let t = step(&HostState::Ready, &mut c, HostEvent::Start);
        assert_eq!(t.next, HostState::Ready);
        assert!(t.effects.is_empty());

        let retryable = HostState::Failed(HostFailure {
            step: HostStep::Deliver,
            error: "flake".into(),
            retryable: true,
        });
        let t = step(&retryable, &mut c, HostEvent::Start);
        assert_eq!(t.next, HostState::Connecting);

        let fatal = HostState::Failed(HostFailure {
            step: HostStep::Consent,
            error: "declined".into(),
            retryable: false,
        });
        let t = step(&fatal, &mut c, HostEvent::Start);
        assert_eq!(t.next, fatal);
        assert!(t.effects.is_empty());
    }

    #[test]
    fn illegal_event_fails_retryable_never_panics() {
        let mut c = ctx(InstallConsent::Ask, vols());
        let t = step(&HostState::Probing, &mut c, HostEvent::ConsentGranted);
        let HostState::Failed(f) = t.next else {
            panic!()
        };
        assert!(f.retryable);
        assert!(f.error.contains("unexpected event"));

        let t = step(&HostState::Ready, &mut c, HostEvent::ImageAbsent);
        assert!(matches!(t.next, HostState::Failed(_)));
    }

    #[test]
    fn resume_matrix() {
        // Fresh Ready ⇒ no-op.
        let (s, fx) = resume(HostState::Ready, Some(1000), 1100, 900);
        assert_eq!(s, HostState::Ready);
        assert!(fx.is_empty());
        // Stale Ready ⇒ re-verify through Connecting.
        let (s, fx) = resume(HostState::Ready, Some(0), 1000, 900);
        assert_eq!(s, HostState::Connecting);
        assert!(fx.contains(&HostEffect::Connect));
        // Never probed ⇒ stale.
        let (s, _) = resume(HostState::Ready, None, 1000, 900);
        assert_eq!(s, HostState::Connecting);
        // Durable mid-states re-enter from the top.
        for st in [
            HostState::Unknown,
            HostState::RuntimeReady,
            HostState::ImageReady,
        ] {
            let (s, fx) = resume(st, None, 0, 900);
            assert_eq!(s, HostState::Connecting);
            assert!(fx.contains(&HostEffect::Connect));
        }
        // Retryable failure retries; fatal stays.
        let retryable = HostState::Failed(HostFailure {
            step: HostStep::Connect,
            error: "x".into(),
            retryable: true,
        });
        assert_eq!(resume(retryable, None, 0, 900).0, HostState::Connecting);
        let fatal = HostState::Failed(HostFailure {
            step: HostStep::Consent,
            error: "declined".into(),
            retryable: false,
        });
        assert_eq!(resume(fatal.clone(), None, 0, 900).0, fatal);
        // A leaked transient re-enters defensively.
        let (s, _) = resume(HostState::Probing, None, 0, 900);
        assert_eq!(s, HostState::Connecting);
    }

    #[test]
    fn durable_tags_round_trip() {
        for st in [
            HostState::Unknown,
            HostState::RuntimeReady,
            HostState::ImageReady,
            HostState::Ready,
        ] {
            let tag = st.durable_tag().unwrap();
            assert_eq!(HostState::from_durable_tag(tag, None), st);
        }
        let f = HostFailure {
            step: HostStep::Deliver,
            error: "x".into(),
            retryable: true,
        };
        assert_eq!(
            HostState::from_durable_tag("failed", Some(f.clone())),
            HostState::Failed(f)
        );
        // failed without meta gets a safe default; junk tags re-provision.
        assert!(matches!(
            HostState::from_durable_tag("failed", None),
            HostState::Failed(_)
        ));
        assert_eq!(HostState::from_durable_tag("wat", None), HostState::Unknown);
        assert_eq!(HostState::Probing.durable_tag(), None);
        assert_eq!(
            HostState::Delivering {
                strategy: DeliveryStrategy::RegistryPull
            }
            .durable_tag(),
            None
        );
    }

    #[test]
    fn next_strategy_walks_the_ladder() {
        let mut c = ctx(InstallConsent::Ask, vols());
        c.strategies = vec![
            DeliveryStrategy::SshStream { rsync: true },
            DeliveryStrategy::SshStream { rsync: false },
            DeliveryStrategy::RegistryPull,
        ];
        assert_eq!(
            next_strategy(&c, &DeliveryStrategy::SshStream { rsync: true }),
            Some(DeliveryStrategy::SshStream { rsync: false })
        );
        assert_eq!(
            next_strategy(&c, &DeliveryStrategy::RegistryPull),
            None,
            "last has no fallback"
        );
        assert_eq!(
            next_strategy(&c, &DeliveryStrategy::RemoteBuild),
            None,
            "unknown strategy has no fallback"
        );
    }

    #[test]
    fn volume_seed_spec_shapes() {
        // The two standard volumes both copy-up seed.
        for v in vols() {
            assert_eq!(v.seed, VolumeSeed::ImageCopyUp);
        }
    }
}
