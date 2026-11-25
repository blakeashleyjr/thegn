# Remote health

## ADDED Requirements

### Requirement: A remote sandbox's live health is readable on demand

thegn SHALL read a remote sandbox's live health with one short read-only exec
inside it, reporting CPU load and cores, memory used against the limit (the
cgroup v2 limit when set, else the VM's total) with memory pressure, disk use
of `/`, uptime, boot id, kernel OOM kills in the current boot, and the time
left before the env's `max_lifetime_secs`. A fact the sandbox cannot report
MUST be shown as unknown, never invented. When the probe cannot run or prints
output that is not a probe's, the health MUST be `unknown` with the reason.

#### Scenario: Fleet inspect with a probe

- **WHEN** the user runs `thegn fleet inspect <sprite> --env <S> --probe`
- **THEN** the report adds health, uptime, boot id, CPU load, memory, disk and
  lifetime left, and `--json` carries the same as a `health` object

#### Scenario: The probe cannot run

- **WHEN** the probe exec fails, times out or exits non-zero
- **THEN** the health is `unknown` and the reason is printed, with no sample

### Requirement: Restarts and OOM kills are named specifically

thegn SHALL classify two samples of one sandbox: a changed boot id is a
restart, likely out of memory when the earlier sample was at or above 90% of
its memory limit or had OOM kills; new kernel OOM-kill lines in the same boot
name the killed process and its resident size. Warnings MUST name the cause
(for example `sprite out of memory: kernel killed rustc 3.7 GB`) rather than a
transport symptom such as a stream ending.

#### Scenario: Reboot after memory pressure

- **WHEN** the previous sample used 3.7 GB of a 4.0 GB limit and the current
  sample has a different boot id
- **THEN** the warning reads
  `sprite restarted: likely out of memory (3.7 GB of 4.0 GB used before the restart)`

#### Scenario: OOM kill without a restart

- **WHEN** the current sample has a kernel OOM-kill line not in the previous
  sample of the same boot
- **THEN** the warning names the killed process and its resident size

#### Scenario: Already-seen kills are not repeated

- **WHEN** both samples of the same boot carry the same OOM-kill lines
- **THEN** no warning is raised

### Requirement: Health is summarised as a level

thegn SHALL summarise a sample as `ok`, `warn` (memory or disk at 90% or
more, or OOM kills this boot) or `unknown` (no sample), and SHALL warn when a
sandbox's lifetime has less than an hour left.

#### Scenario: Memory near the limit

- **WHEN** a sample's memory is at 95% of its limit
- **THEN** its level is `warn`
