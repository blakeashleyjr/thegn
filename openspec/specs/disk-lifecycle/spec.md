# disk-lifecycle Specification

## Purpose

TBD - created by archiving change reclaim-idle-worktree-targets. Update Purpose after archive.

## Requirements

### Requirement: Abandoned worktrees release their build artifacts

thegn SHALL reclaim a worktree's `target/` once nothing in that worktree —
source files or build output — has been modified for `[disk] idle_clean_days`
days. The checkout itself MUST be kept; only regenerable build artifacts are
removed. `idle_clean_days = 0` SHALL disable the rule.

The rule MUST NOT act on the active worktree, a worktree with a thegn-spawned
build or test running, a worktree with uncommitted changes, or a `target/`
smaller than the module's minimum-worth-reclaiming floor. These exemptions exist
because an unexpected cold rebuild is a real cost to work in flight; the rule
targets abandonment, which the existing merge/close rules do not cover.

The decision SHALL be a pure function of already-measured facts (path, `target/`
bytes, seconds since the newest modification, and the active/building/dirty
flags), so it is testable without a filesystem.

#### Scenario: A worktree nobody has touched for weeks

- **WHEN** a worktree with a multi-GiB `target/`, no running build, and no
  uncommitted changes has had nothing modified in it for longer than
  `idle_clean_days`
- **THEN** its `target/` is reclaimed, the checkout is left intact, and a
  notification records the bytes recovered and that the reason was idleness

#### Scenario: Work in flight is never surprised

- **WHEN** the idle threshold is exceeded but the worktree is the active one, has
  a running build, or has uncommitted changes
- **THEN** nothing is reclaimed

#### Scenario: The rule is switched off

- **WHEN** `idle_clean_days` is 0
- **THEN** no worktree is reclaimed on idleness however old it is

### Requirement: Disk pressure evicts least-recently-used build artifacts

When `[disk] reclaim_on_low_disk` is enabled and free space on the worktrees'
filesystem is at or below `[stats] disk_free_critical`, thegn SHALL evict
worktree `target/` dirs least-recently-touched first, stopping as soon as enough
bytes have been selected to bring free space back above `[stats] disk_free_warn`.

This rule SHALL reuse the existing free-space thresholds rather than introduce an
absolute size budget: an absolute total is permanently exceeded on a machine that
runs many worktrees, and a threshold that is always tripped carries no
information. `[disk] warn_threshold_gb` therefore remains a reporting threshold
for `thegn disk` and MUST NOT drive automatic reclaim.

Unlike the idle rule, uncommitted changes SHALL NOT exempt a worktree — pressure
is an emergency. The active worktree, a worktree with a running build, and any
worktree touched within the recent-activity window MUST still be exempt.

#### Scenario: The filesystem crosses its critical line

- **WHEN** free space is at or below `disk_free_critical` and several worktrees
  hold reclaimable `target/` dirs
- **THEN** the least-recently-touched ones are reclaimed, in that order, only as
  far as is needed to reach `disk_free_warn`, and the notification records that
  the reason was low disk

#### Scenario: There is room on the disk

- **WHEN** free space is above `disk_free_critical`
- **THEN** no worktree is evicted for pressure, however large its `target/`

#### Scenario: An agent is mid-task

- **WHEN** the filesystem is under pressure but a candidate worktree was modified
  within the recent-activity window
- **THEN** that worktree is skipped and the next least-recently-touched one is
  considered instead

### Requirement: Reclaim runs off the event loop and explains itself

The reclaim pass SHALL run on the existing background measurement lane at the
tail of a disk-scan round — never on the event loop, never before the first
frame, and adding no new wake source. It MUST decide only from measurements taken
in that same round, so no reclaim is ever made from a stale size or mtime.

Every reclaim SHALL record a `disk_cleaned` notification naming the worktree, the
bytes recovered, and which rule fired, so the next attach can account for the
cold rebuild rather than presenting it as unexplained.

Turning off the per-worktree size badges MUST NOT disable reclaim: visibility and
lifecycle are separate settings.

#### Scenario: Size badges are hidden

- **WHEN** `[disk] show_sizes` is false and at least one reclaim rule is enabled
- **THEN** the background round still measures and still reclaims

#### Scenario: A round reclaims something

- **WHEN** a background disk-scan round reclaims a worktree's `target/`
- **THEN** the size cache row for that worktree is dropped, a `disk_cleaned`
  notification carrying the byte count and the reason is written, and the UI is
  woken to repaint

### Requirement: Build-lane inventory is read-only and evidence-based

`thegn wt disk --lanes` SHALL inventory the immediate subdirectories of one
checkout's `target/lanes/` and report, per lane, its apparent-byte size, the
regenerable build output inside it, the owning worktree it can verify, its
activity evidence, an eligibility class, and a reason. Per-lane and aggregate
sizes MUST use the same apparent-byte accounting as `thegn disk`, so a lane's
`target/` and a worktree's `target/` are the same unit; only lanes classified
`eligible` contribute to the eligible total.

Ownership MUST come from the lane's own git checkout. A lane with no checkout of
its own SHALL be reported with unknown ownership rather than attributed to the
checkout that contains it, because ownership is what makes a lane reclaimable.
Activity MUST be Cargo's own per-profile `.cargo-lock` — held, never waited on —
or the newest write anywhere in the lane, using the same recent-activity window
as low-disk eviction. A lane SHALL be eligible only when it is a build lane, has
a verifiable owner, holds no build lock, and has been idle longer than that
window; every other lane MUST be reported `held` or `unknown` with its reason.

The command MUST NOT remove or modify lane contents, write the `worktree_disk`
cache, or scan any other worktree.

#### Scenario: Active, idle, locked, and unowned lanes

- **WHEN** the inventory reads a lane written moments ago, one idle for days, one
  whose Cargo lock is held, and one with no checkout of its own
- **THEN** it reports the fresh and locked lanes as `held`, the verified idle
  lane as `eligible`, and the unowned lane as `unknown`, prints a reason for
  each, and leaves every lane's contents byte for byte intact

#### Scenario: A checkout has never coordinated a lane

- **WHEN** the selected checkout has no `target/lanes/` directory
- **THEN** the inventory reports no lanes and exits successfully
