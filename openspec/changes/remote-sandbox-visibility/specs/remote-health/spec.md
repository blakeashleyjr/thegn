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

### Requirement: A background sampler watches running sandboxes

thegn SHALL probe each running remote worktree sandbox about once a minute
while a client is attached, store the latest and previous reading in the state
DB, and MUST NOT probe a sandbox the provider reports cold, warm, stopped or of
unknown state. A probe that fails or times out MUST store no sample and back
that sandbox off for 5 minutes. Restart and OOM events, and a lifetime under
one hour, SHALL each raise one notification and one `remote_events` journal row
(500 rows and 30 days per sandbox). A restart whose cause is otherwise unknown
SHALL be named from provider facts: idle sleep when the provider showed the
sandbox cold between samples, lifetime expiry when it is past
`max_lifetime_secs`.

#### Scenario: A cold sandbox

- **WHEN** the provider reports a sandbox cold
- **THEN** the sampler runs no exec in it and stores no sample

#### Scenario: An event is raised once

- **WHEN** a restart is classified and the next sample is of the same boot
- **THEN** one notification and one journal row exist for it, and none is
  added by the later sample

### Requirement: A lost exec stream names its cause

When an exec stream ends without an exit status, thegn SHALL run one fresh
probe, and when the boot id differs from the stored previous reading it MUST
print the specific restart warning instead of `exec stream ended without an
exit status`.

#### Scenario: Stream ends across a reboot

- **WHEN** a sprite exec stream drops and the probe shows a new boot id
- **THEN** the line printed is the classified warning, not the bare message
