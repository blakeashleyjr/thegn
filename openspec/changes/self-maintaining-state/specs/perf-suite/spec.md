# Perf Suite

## MODIFIED Requirements

### Requirement: Benchmarks are opt-in and excluded from CI

The pure planner simulation SHALL be the idle regression gate for the background scanners, since a one-minute soak window is shorter than one LOC sweep. The simulation SHALL fail if any lane cannot reach idle for a registry size up to 1,000 worktrees.

#### Scenario: A lane never idles

- **WHEN** a planner change leaves a lane re-planning between sweeps
- **THEN** the simulation test fails
