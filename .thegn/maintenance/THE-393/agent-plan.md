# THE-393 plan

Scope: crates/thegn-svc/src/git/plumbing.rs only.
Replace stderr "but expected" match in update_ref_cas with a re-read of the ref
(rev-parse --verify --quiet, exit-code based) -> typed CasOutcome
{Advanced, Moved, Missing, Other}; pure classify fn; Unknown/still-at-old fails
closed (Err). update_ref_cas keeps its bool signature (Moved=false), so callers
in integrate.rs are unchanged.
Tests: pure classifier (incl. stderr containing old phrase is not Moved), real-repo
missing ref and stale ref.
