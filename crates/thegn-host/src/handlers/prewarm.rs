//! Shared automatic-prewarm resolution policy.
//!
//! The worker owns the slow external effects, while this helper owns the
//! ordering that makes automatic prewarm safe: reject a host result before a
//! remembered-agent fold, probe for an already-live daemon session, apply the
//! fold, then reject again.  Keeping that ordering in one callable seam lets
//! the executable regression exercise the route without a live provider or
//! daemon.

use crate::agent::LaunchSpec;
use crate::handlers::provision::SpecError;
use crate::handlers::worktree_attach::AttachTarget;

pub(crate) type Specs = Result<Vec<(u32, LaunchSpec)>, SpecError>;

/// Resolve one automatic prewarm request around injected slow effects.
///
/// `resolve` is the actual sandbox/provider/terminal resolution selected by
/// the worker, `probe` is the connect-only daemon probe, and `relaunch` is the
/// remembered-agent fold.  The two host gates deliberately surround the fold:
/// resurrection must not turn a safe contained shell into an automatic host
/// launch.
pub(crate) fn resolve_automatic_with<R, P, L>(
    first_leaf: Option<u32>,
    resolve: R,
    probe: P,
    relaunch: L,
) -> (Specs, Vec<AttachTarget>)
where
    R: FnOnce() -> Specs,
    P: FnOnce() -> Vec<AttachTarget>,
    L: FnOnce(&mut Specs, Option<u32>, bool),
{
    let mut specs = resolve();
    crate::agent::reject_host_prewarm(&mut specs);
    let attach = probe();
    relaunch(&mut specs, first_leaf, attach.is_empty());
    crate::agent::reject_host_prewarm(&mut specs);
    (specs, attach)
}
