# Sandbox and remote programme: wave-0 spike procedures

One procedure page per wave-0 spike or on-device check (21 items), each runnable as a single
sitting once the needed device or account is available. These are **preparation only**: nothing here
has been run. The decision records the issues name (`docs/audits/...`, `openspec/changes/.../`,
`config/recipes/connector/VERIFICATION.md`) are written after the run, from the evidence.

Programme rules these pages serve: B8 (no item whose design rests on an unverified external fact merges
before its spike's decision record) and the hardware/accounts checklist in
[`docs/superpowers/plans/2026-11-17-sandbox-remote-programme.md`](../../plans/2026-11-17-sandbox-remote-programme.md) section 4.
A check that needs a device or vendor account is recorded in the PR with the host and date
([`CONTRIBUTING.md`](../../../../CONTRIBUTING.md) "Programme conventions").

## Conventions

- **Layout.** Procedures: `docs/superpowers/specs/sandbox-remote-spikes/<ITEM>.md` (this directory).
  Scripts: `scripts/spikes/<item-lowercase>.sh` or `.ps1`, shared helpers `lib.sh`, `lib-cases.sh` (bash
  3.2-safe JSON-lines case runner for the Mac), `lib.ps1`. Throwaway Rust probes:
  `scripts/spikes/probes/<name>/` (below). CONTRIBUTING prescribes no other location.
- **Run a script.** `scripts/spikes/<item>.sh OUTDIR` (Windows: `pwsh -File scripts\spikes\<item>.ps1 -OutDir DIR`).
  Convention: `OUT=$HOME/spike-evidence/<ITEM>-$(date +%F)`. It writes ONE evidence file
  `OUTDIR/<ITEM>.evidence.txt` (`.jsonl` for the macOS case-runner items), raw captures only where the page asks for
  fixtures under `OUTDIR/<ITEM>/`, exits non-zero on a failed precondition (2), removes whatever it created
  (trap / `finally`) and prints `VERDICT (<ITEM>): ...` when the decision rule is computable.
- **Secrets** are read from named environment variables (`DAYTONA_API_KEY`, `HCLOUD_TOKEN`,
  `MACHINE0_API_KEY`) and handed to curl through a 0600 config file; never on argv, never saved.
  Redact user names and home paths (`lib.sh` does); review every capture before committing it.
- **Rust probes** are standalone crates, each with an empty `[workspace]` table in its own `Cargo.toml`,
  so they never join the thegn workspace and never touch `Cargo.lock` (the root `exclude = ["apps", "vendor"]`
  needed no change). The scripts build a COPY under OUTDIR; a `.gitignore` here keeps `target/` and `Cargo.lock` out.
  Probes: `confine-apis` (WS03-P0-01), `exec-cost` (WS03-P0-02), `mux-bench` (WS09-P0-04), `plugin-framing` (WS10-P2-04).
- **Evidence tags** for the records: `verified on device/host`, `source read`, `vendor docs`, `still unverified`.
- **Not scripted, and why** (marked in each page): WS06-P4-00 (X1-X5 need undocumented-ABI code taken from the
  pinned MXC source, so only the precondition probe is scripted); WS09-P0-03 account rows and WS06-P2-00 C3/C4/C7;
  WS10-P7-01 (a source-reading aid; the decisions are human); WS03-P0-04 per-cache "verified on use" table; WS06-P0-05
  checks 2.2-2.6/2.9; the implementation halves of WS05-P0-01, WS06-P6-03, WS08-P0-03 and WS02-P3-08 (the repo
  files those items land: `test/macos/`, `just` recipes, openspec changes, `Mcp0Client::list_tools`).

## Index, grouped by what each needs, in recommended run order

Run order inside a group is top to bottom; groups are independent apart from the dependencies noted.

### 1. Needs nothing (a Linux dev host; run these first, they unblock the most)

| #   | Item                        | Question                                                                                                                     | Script                                     | Time              |
| --- | --------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------ | ----------------- |
| 1   | [WS03-P0-01](WS03-P0-01.md) | Do landlock 0.4.7 / seccompiler 0.5.0 provide each API symbol on an ABI 10 host (and fit MSRV 1.89)?                         | `ws03-p0-01.sh` + `probes/confine-apis`    | 1 h               |
| 2   | [WS03-P0-02](WS03-P0-02.md) | Is a self-exec helper within 8 ms p95 of `/bin/true`, or ship a small static `thegn-confine-exec`? (x86_64 and aarch64)      | `ws03-p0-02.sh` + `probes/exec-cost`       | 1.5 h per machine |
| 3   | [WS09-P0-04](WS09-P0-04.md) | Which mux credit window (64/256/1024 KiB) keeps echo p99 under flood within 2x idle; optimistic OPEN gain?                   | `ws09-p0-04.sh` + `probes/mux-bench`       | 1 h               |
| 4   | [WS10-P2-04](WS10-P2-04.md) | Is base64-in-NDJSON fast enough for plugin streams, or is a `stream:fd` side channel needed?                                 | `ws10-p2-04.sh` + `probes/plugin-framing`  | 45 min            |
| 5   | [WS09-P0-02](WS09-P0-02.md) | Which connector-recipe flags and transport facts hold on Linux (dumbpipe ticket, kubectl exec, git bundle matrix, decoders)? | `ws09-p0-02.sh`                            | 2-3 h             |
| 6   | [WS09-P0-05](WS09-P0-05.md) | Ship the full musl `thegn` or a feature-trimmed `thegn-remote` (<= 40% raw size + smoke)?                                    | `ws09-p0-05.sh` (measures builds you made) | 2-3 h             |
| 7   | [WS03-P0-04](WS03-P0-04.md) | Is bwrap overlay or reflink CoW safe/fast for build dirs and caches; which caches are verified on use?                       | `ws03-p0-04.sh` (per filesystem)           | 3-4 h             |
| 8   | [WS10-P7-01](WS10-P7-01.md) | What do the E2B / OpenSandbox / Runloop / Modal protocols look like; OpenSandbox PTY? which SaaS reference vendor?           | `ws10-p7-01.sh` (reading aid)              | 3-5 h             |

### 2. Needs an Ubuntu host with AppArmor (24.04 and 26.04 VMs, plus the GitHub `ubuntu-24.04` runner)

| #   | Item                        | Question                                                                                   | Script                                         | Time            |
| --- | --------------------------- | ------------------------------------------------------------------------------------------ | ---------------------------------------------- | --------------- |
| 9   | [WS03-P0-03](WS03-P0-03.md) | What do bwrap / userns probes print per distro, and which `UsernsStatus` does each map to? | `ws03-p0-03.sh OUTDIR CASE` (one run per host) | 15 min per host |

### 3. Needs the owner's Apple-silicon Mac (macOS 26; macOS 15 for Seatbelt; KVM Linux for smolvm)

Do these in one Mac session in this order (same device; WS05-P0-01's runner is shared).

| #   | Item                        | Question                                                                                                                                                                        | Script                                 | Time                    |
| --- | --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------- | ----------------------- |
| 10  | [WS05-P0-01](WS05-P0-01.md) | What do Apple `container` (`network none`, internal network, `--ulimit`, labels, `--version`) and podman/docker print on macOS?                                                 | `ws05-p0-01.sh`                        | 15 min + review         |
| 11  | [WS05-P0-06](WS05-P0-06.md) | Do the 19 Seatbelt primitives hold (parameters, pty rules, firmlinks, home links, unix sockets, loopback, violation log)? macOS 26 and 15                                       | `ws05-p0-06.sh` (interactive terminal) | 45 min per OS           |
| 12  | [WS07-P2-01](WS07-P2-01.md) | Does smolvm behave as the source says on 16 checks (path-preserving volume, ownership, network, secrets, fork, resident processes, read-only rootfs)? Linux+KVM first, then Mac | `ws07-p2-01.sh`                        | 1 h per host            |
| 13  | [WS02-P3-08](WS02-P3-08.md) | What do real container runtimes print (docker/podman/nerdctl/finch, events) so parsers use real fixtures? Linux now; Mac per product                                            | `ws02-p3-08.sh`                        | 20 min Linux; 2-3 h Mac |

(WS02-P3-08's Linux half needs nothing and can run in group 1; its macOS half belongs to the Mac session.)

### 4. Needs a Windows device and KVM (Win11 24H2 26100.9278+ VM with nested virtualisation, the owner's Windows box, a Windows 10 VM for refusal checks)

Order matters: the VM spike decides where WSL work runs, then the baseline gate, then the rest in one device session.

| #   | Item                        | Question                                                                                                             | Script                                                     | Time     |
| --- | --------------------------- | -------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------- | -------- |
| 14  | [WS06-P6-03](WS06-P6-03.md) | Does WSL2 boot inside the Win11 KVM guest (so WSL/engine checks can run in the VM)?                                  | `ws06-p6-03.sh` (Linux host)                               | 2-3 h    |
| 15  | [WS06-P0-05](WS06-P0-05.md) | Does native Windows build, render, handle Ctrl+C and the daemon race (baseline gate)?                                | `ws06-p0-05.ps1` + manual checks + opt-in CI run           | 2-4 h    |
| 16  | [WS06-P2-00](WS06-P2-00.md) | How does WSL really behave (list encoding, registry, PTY, idle, WSLENV, export/import, arg limits)?                  | `ws06-p2-00.ps1` (partial)                                 | 1.5-2 h  |
| 17  | [WS09-P0-03](WS09-P0-03.md) | Are `ssh.exe` / `wsl.exe -e` stdio binary-safe; Win32 ProxyCommand and ControlMaster facts? (account rows later, W5) | `ws09-p0-03.ps1` (Windows half)                            | 1-1.5 h  |
| 18  | [WS06-P4-00](WS06-P4-00.md) | Direct PSEC exports or pinned `mxc-sdk`; ConPTY under PSEC; proxy peer identity; userinfo? Both 26100 and 26200      | `ws06-p4-00.ps1` (precondition only) + unmerged spike code | 3-5 days |

### 5. Needs a paid or vendor account (cheapest and read-only first)

| #   | Item                          | Question                                                                                                  | Script                      | Cost / time                                               |
| --- | ----------------------------- | --------------------------------------------------------------------------------------------------------- | --------------------------- | --------------------------------------------------------- |
| 19  | [WS08-P0-03](WS08-P0-03.md)   | Do the 12 hard-coded machine0 MCP tool names exist in the live `tools/list`?                              | `ws08-p0-03.sh` (read-only) | none / 15 min                                             |
| 20  | [WS08-P0-01s](WS08-P0-01s.md) | How does the Daytona ssh gateway mint tokens, run commands and PTYs, with the token never in argv?        | `ws08-p0-01s.sh`            | one throwaway sandbox / 45 min                            |
| 21  | [WS08-P0-02](WS08-P0-02.md)   | Does the Hetzner VPS lifecycle work on a funded account without leaking servers (flip `live_unverified`)? | `ws08-p0-02.sh`             | **real billable servers**, cents to a few euros / 1.5-2 h |

## Items that could not be fully scripted

| Item                   | Scripted                                               | Left to a human                                                                            |
| ---------------------- | ------------------------------------------------------ | ------------------------------------------------------------------------------------------ |
| WS06-P4-00             | build/UBR table, `processmodel.dll` export presence    | X1.2-X1.5, X2-X5 (undocumented ABI from MXC source; unmerged spike code)                   |
| WS06-P2-00             | C1, C2, C5, C6, C8, C10, C11, C3 non-interactive       | C3 resize/Ctrl+C, C4, C7, C9 on a policy VM, localized/WSL1 C1 states                      |
| WS09-P0-03             | Windows half                                           | all account rows; quoting goldens (not yet written)                                        |
| WS06-P0-05             | env, CI status, 2.1, 2.7, 2.8                          | 2.2-2.6, 2.9; dispatching CI; triage and fixes                                             |
| WS06-P6-03             | the nested-virt spike evidence                         | VM provisioning, upgrade, runner (implementation)                                          |
| WS03-P0-04             | timings, upper growth, hardlinks, inotify, correctness | per-cache verified-on-use table; Node corpus                                               |
| WS10-P7-01             | clones + greps                                         | every decision                                                                             |
| WS09-P0-05             | measurements                                           | the throwaway feature split, builds, smoke subset                                          |
| WS03-P0-01             | one build per symbol + runs                            | triage of compile errors (the UDP variant names are a best guess), the record              |
| WS07-P2-01             | 16 checks, most auto-judged                            | REVIEW rows, the `-i -t` PTY exec, openspec authoring                                      |
| WS05-P0-01, WS02-P3-08 | the evidence runs                                      | the repo files those items land (`test/macos/`, recipes, openspec change, fixtures review) |

## Gate and ratchet notes for this directory

`scripts/` and `docs/superpowers/` are outside the nix source allowlist (`nix/source.nix`), so no package
rebuild; the scripts are covered by `just lint` (`git ls-files '*.sh' | shellcheck -x`) and are shfmt/rustfmt-formatted
(2-space, edition 2024). The `test/*-ratchet.txt` greps scan `crates/` only. The probes stay outside the cargo workspace.
