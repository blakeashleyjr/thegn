# CI Inspection

## Purpose

thegn surfaces CI/CD pipeline state — run history, job/step drilldown, and logs
with jump-to-failure — across providers through a provider-agnostic model. CI is a
separate axis from the forge: the `CiProvider` trait is a sibling of the forge
trait, providers degrade native-API → CLI → unavailable, and the whole layer is
AI-free.

## Requirements

### Requirement: Provider-agnostic run/job/step/log model

CI SHALL be modeled as a normalized `run → job → step → log` shape behind a `CiProvider` trait, and each provider MUST degrade native-API → CLI → unavailable, with mutating operations capability-gated.

#### Scenario: Provider lacks native API

- **WHEN** a provider has no native API available but a CLI is present
- **THEN** the provider serves runs/jobs/logs via the CLI

#### Scenario: Mutation not supported

- **WHEN** a trigger/rerun/cancel is requested on a provider that does not declare
  the capability
- **THEN** the mutation is refused rather than attempted

### Requirement: CI rollup in the panel and statusbar

CI state SHALL appear as a panel `Ci` section (recent runs + per-run state + summary chip) and a statusbar badge that is red on failure, amber while running, and silent when green.

#### Scenario: A run is failing

- **WHEN** the latest CI run has failures
- **THEN** the statusbar shows a red CI badge and the panel section reflects the
  failure

### Requirement: CI refresh runs off the event loop

CI cache refresh SHALL run off-loop (`spawn_blocking` + channel + `TerminalWaker`) on switch and on a bounded interval, writing the `ci_runs_cache`, preserving the ~0% idle invariant.

#### Scenario: Background CI poll

- **WHEN** CI data is refreshed
- **THEN** it runs off-loop and wakes the loop only with new data, adding no
  polling timeout

### Requirement: "Why did it fail" is AI-free

Failure explanation SHALL be a log scan that marks the first failure line (error markers / exit codes / panics), with no LLM involvement.

#### Scenario: First-failure marker

- **WHEN** the user views a failed run's log
- **THEN** a "first failure at line N" marker is shown from a deterministic scan
