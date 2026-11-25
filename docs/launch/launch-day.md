---
title: Launch-day operations, Monday 2026-11-30
type: feat
status: active
date: 2026-11-24
---

# Launch-day operations, Monday 2026-11-30

The launch is the Show HN post at **Monday 2026-11-30, 06:00 PT** (09:00 ET,
14:00 UTC). Every time below is PT. This page is the day-of plan: post times,
who watches what, the quick-fix release path, the prepared answers, and the
pinned start-here discussion.

Inputs are the launch kit researched on 2026-11-24 — `show-hn.md`,
`launch-checklist.md`, `references.md`, `blog-outline.md`,
`comparison-draft.md`, `capture-plan.md`, `feature-inventory.md`,
`readme-skeleton.md`, and the four claim audits under `claims/`. The kit lives
outside the tree; only what launch day itself needs is restated here. Facts
below were re-verified against this tree at `0.1.0-alpha.132` (2026-11-24);
where the kit and the tree disagree, the tree wins. Three things already
moved since the kit was written: the local merge queue is on by default, the
macOS wording is reconciled to "supported, sandbox best-effort", and Homebrew
is a live channel.

2026-11-30 is Cyber Monday. There is no data on its effect on HN traffic, so
treat the slot as untested and rely on presence in the thread, not on the
exact minute.

## Preconditions

Weekend gates from the launch checklist. If one is red on Sunday night, fix it
before bed; nothing on this list is fixable at 05:15 on Monday.

- The launch tag is **published** (not a draft) and marked latest, with the
  GNU, musl and aarch64-darwin archives, checksums and attestations, and
  `thegn --version` matches the release page. Decide by Fri 11-27 whether to
  cut a fresh alpha so the launch tag is newer than the last few days' fixes.
- `just net-trace` has been run on the release binary and its summary recorded
  in `docs/help/privacy.md`. That page still says the first-run trace results
  are pending, so the telemetry answer must not outrun it.
- The blog post is scheduled (or ready to flip) for 05:45 Monday, and the
  landing site is deployed, HTTPS, link-checked, OG card rendering.
- Discussions are enabled (categories Q&A, Ideas, Show and tell) and the
  start-here discussion is posted and pinned (see
  [Pinned start-here discussion](#pinned-start-here-discussion)). The launch
  release is pinned too.
- The first HN comment is pre-written in a scratch file with the real values
  filled in; the skeleton is in the kit's `show-hn.md`.
- The README is final and the install one-liners were verified from a clean
  Linux machine and a clean Mac, as a stranger would follow them.
- Speed language is decided: unless `just bench-idle`, `just perf-t3` and
  `just bench-soak` numbers were recorded and published, no speed adjectives
  appear anywhere in public copy.
- The competitor cells in the comparison table were re-verified against each
  project's current README (the kit's `comparison-draft.md` marks the
  unverified ones).
- Two alarms are set. Sleep.

## Post times, Monday 2026-11-30

| Time        | Action                                           | Done when                                                                                                                                            |
| ----------- | ------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| 05:15       | Pre-flight from a different machine and network. | Release published and latest; site live; README renders; install one-liners work; blog scheduled; HN account in good standing.                       |
| 05:45       | Publish the blog post.                           | Canonical link loads; OG card and images render; the site's install block matches the README.                                                        |
| 06:00       | Submit Show HN.                                  | Repo URL as the link; title from below; no tracking parameters; no voting link anywhere.                                                             |
| 06:01–06:05 | Post the first comment.                          | What it is, why, how it differs, what is rough, one-command try, feedback wanted — real values, no bracket left.                                     |
| 06:00–08:00 | Full presence in the thread.                     | Every comment answered quickly; valid criticism conceded; nothing waits more than ~30 minutes.                                                       |
| 08:00–12:00 | Thread watch plus hourly triage.                 | Issues triaged on the hour; cheap visible fixes shipped; the FAQ scratch file is growing.                                                            |
| ~12:00      | Cross-post decision.                             | At most one post per channel (r/commandline, lobste.rs, own socials), only if wanted, after the HN morning window, never as a vote push.             |
| Afternoon   | Release decision point.                          | Any hotfix release is dispatched by 15:00 or explicitly deferred to Tuesday.                                                                         |
| Evening     | Wrap-up.                                         | What worked and what broke is written down; README, FAQ and KNOWN_ISSUES.md updated from the thread; the pinned discussion gets a dated day-1 recap. |

### Show HN specifics

- Submit the repo URL, not the blog and not the site.
- Recommended title, 73 characters including `Show HN: `:

  ```text
  Show HN: Thegn – A terminal multiplexer where every tab is a git worktree
  ```

  Measured alternates from the kit: "…– Terminal multiplexer for coding
  agents, with sandboxed panes" (77) leads with security and invites the
  saturated "agent manager" framing; "…– Terminal workspace with a sandbox per
  git worktree" (67) is the shortest. Avoid the "10 parallel agents" framing;
  "review is the bottleneck" was the top criticism of similar launches.

- Post the first comment within 1–2 minutes. Concede what is rough before
  anyone else does; the kit's `show-hn.md` has the skeleton.
- Never ask for upvotes and never share a voting link. Do not resubmit the
  same day if the post stalls; answers in a quiet thread still become FAQ
  material.

## Who watches what

This is a solo launch: one operator, two hats, so the thread and the tracker
cannot silently starve each other.

- **Thread hat** — the HN item, then its `new` and second-chance pages, plus
  any afternoon cross-post threads. Reply fast, concede valid criticism, stay
  plain. Wear it continuously 06:00–08:00, then check every ~30 minutes.
- **Tracker hat** — GitHub Issues (they arrive via the templates), the
  Discussions, and mentions. Wear it on the hour, 08:00 until close: label,
  reproduce or ask for repro, answer or file.

Coverage rules: the thread wins 06:00–08:00 and the tracker is checked only at
07:00 and 07:30; after 08:00 take ten minutes off every hour and eat at noon;
phone notifications carry both hats in between.

Watchlist — the failure classes the kit and the claim audits expect, with the
first response for each.

| Signal                                                                                  | First response                                                                                                                                                                                                        |
| --------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Alt chords do nothing in an unexercised terminal (kitty, WezTerm, iTerm2, Terminal.app) | Only Ghostty and Alacritty are exercised; ask for terminal and version; point at `docs/help/terminal-compatibility.md`; label `compat`.                                                                               |
| Mac first-run: pane refused, no sandbox backend                                         | That is by design — there is no host fallback. `thegn doctor` names the missing backend (Apple `container`, podman or docker); macOS sandboxing is best-effort per `docs/help/known-limitations.md`; label `install`. |
| Mac: Gatekeeper hang on the browser-downloaded tarball                                  | `xattr -dr com.apple.quarantine ./thegn` **before** first launch; a denied verdict is cached, so recovery then means System Settings or a fresh extract; Homebrew installs are not quarantined.                       |
| Mac: Alt/Option inserts characters                                                      | Set Option as Meta/Alt; `thegn doctor` names the setting.                                                                                                                                                             |
| `gh auth login` friction on the tarball path                                            | The README already offers the browser-download alternative; if it keeps recurring, add a plain `curl` line to the README in the afternoon.                                                                            |
| Upgrade refusal: refusing database schema migration                                     | The old daemon holds the schema lease: `thegn daemon stop`, then relaunch; `just update stable <version>` does the whole sequence for the owner's instance. Label `install`; candidate line for KNOWN_ISSUES.md.      |
| Windows questions                                                                       | Deferred, no binary; WSL2 with the Linux x86_64 archive; the native MSVC port compiles but its first CI run failed in the named-pipe IPC tests.                                                                       |
| A sandbox escape claim                                                                  | Do not argue in the thread. Reproduce quickly, then use the private channel in [SECURITY.md](../../SECURITY.md) and reply publicly only that it went to the security channel.                                         |
| Crash reports                                                                           | Ask for `thegn doctor` output, and for `thegn doctor bundle --out thegn-bundle.tar.gz` only with the redaction caveat from [SUPPORT.md](../../SUPPORT.md).                                                            |

Everything anyone asks, anywhere, goes into the FAQ scratch file. Questions
become FAQ entries, bugs become labeled issues (use the existing set in
`.github/labels.yml`: `launch-blocker`, `install`, `compat`, `known-issue`,
`needs-repro`, `confirmed`), and requests become Discussions.

## Quick-fix release path

Decision ladder, cheapest first. Always rung 1; rung 3 only for a bug that
bites many users or blocks install.

1. **Answer.** A comment with the workaround — most launch-day reports are the
   watchlist items above. Minutes.
2. **Doc fix plus pinned comment.** Edit the README or KNOWN_ISSUES.md, then a
   pinned comment pointing at the change. Under an hour, no release.
3. **Hotfix release `0.1.0-alpha.N+1`.** The condensed path below, from
   [RELEASING.md](../../RELEASING.md) (which stays normative). Budget 2–3
   hours from decision to published; the answer rung runs first so users are
   unblocked while it does.

The release path:

1. Fix on a worktree; `just quick <crate>` plus the focused
   `cargo nextest run -p <crate> <filter>`.
2. Bump `[workspace.package] version`, run `cargo build --workspace` to
   refresh `Cargo.lock`, and move the `## [Unreleased]` CHANGELOG items under a
   dated heading with the compare links.
3. Land on `main` via `thegn land` or the merge queue, and pull.
4. `just nix-build-full` — the routine gate only builds `.#thegn-nobridge`,
   and users get `.#default` with the bridge.
5. `just release-artifacts v<version>` and `just release-verify v<version>`;
   the bridge binary is required and a stale or debug build is refused.
6. Annotated tag `v<version>`, push. Approve `release-admission`; the upload
   broker publishes the draft; `verify-manifest` must end green.
7. Release notes from the RELEASING.md template, then dispatch
   `publish-release`:

   ```sh
   gh workflow run release.yml --ref main -f tag=v<version> -f operation=publish-release
   ```

8. Dispatch `operation=publish-external` for the Homebrew tap
   (`blakeashleyjr/homebrew-thegn`) and approve
   `package-publication-homebrew` when prompted.

Hard rules, all from RELEASING.md:

- Retry a failed leg with "Re-run failed jobs" **on the original run**; a fresh
  run cannot take over that tag's draft.
- Never publish a draft from the GitHub UI; UI publication is break-glass only
  and requires stopping every queued or running workflow for the tag first.
- A published release is never deleted and a tag is never moved; a mistake
  gets a new tag.
- Nothing dispatches after 15:00 unless it is install-blocking, because an
  evening release finishes unattended.

Time-to-users shortcuts while a release runs: the Nix flake installs off any
ref with no release (`nix profile install github:blakeashleyjr/thegn/<ref>`),
and the two stopgaps that close most reports need no release at all —
`thegn daemon stop` after an upgrade, and the macOS `xattr` line before a
first launch.

## Prepared answers

Rules of engagement first; they are what the breakouts in the kit's research
had in common:

- Reply to every comment quickly; concede valid criticism; never ask for
  votes. Founder presence for 2+ hours matters more than the exact minute.
- Plain voice; no sentence a generic AI post could contain. Every fact below
  carries its source path in this tree.
- `[OWNER]` marks the slots only the maintainer can fill; nothing goes out
  with a bracket left in it.

| Question                                       | Answer                                                                                                                                                                                                                                                                                                                                                                                                                                                  | Source                                                                 |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| Why not tmux / zellij?                         | tmux is the right tool on a bare server and the docs say so; thegn adds worktree tabs, the diff/PR/CI panel and sandboxed panes, and it runs inside tmux with `extended-keys on`. No layout files, no WASM plugins — plugins are out-of-process JSON programs. Do not claim it is faster.                                                                                                                                                               | `docs/help/comparison.md`, `docs/help/plugins.md`                      |
| Why parallel agents? Review is the bottleneck. | Agree. The merge queue, the diff panel and PR review are the product — the part after the agent stops. The queue is on by default and gated by `gate_command`.                                                                                                                                                                                                                                                                                          | `docs/help/merge-queue.md`                                             |
| What does the sandbox stop?                    | `standard` is an accident guard: read-only outside the worktree, and it hides well-known credential files and sibling repositories, but the rest of `$HOME` stays readable, the network is direct, and `~/.keychain` and `~/.claude-profiles` stay writable. `sealed` hides home, cuts the network and drops capabilities, but shares read-write toolchain caches; no private-copy workspace exists yet, and batch and merge-gate jobs run on the host. | `docs/help/sandbox-profiles.md`, `docs/help/sandbox-security-model.md` |
| How was the sandbox tested?                    | Linux bwrap end to end: `just security-e2e` and `just sandbox-userns-e2e` plus the `sandbox_*` suites in `thegn-core`. The `apple` backend has unit tests only, no runtime coverage. `[OWNER: one line on what is untested.]`                                                                                                                                                                                                                           | `KNOWN_ISSUES.md`, `test/`                                             |
| Telemetry?                                     | No analytics or crash-upload client in the source, no update check, crash reports stay local. The AI-usage gauge is on by default and may call the provider with the agent CLI's own login; `[usage] enabled = false` turns it off. Do not claim "no network calls": git auto-fetch, `gh`/`glab`, usage polling and media cover art touch the network, and the first-run trace result goes into `privacy.md` before launch.                             | `docs/help/privacy.md`, `docs/help/ai-usage.md`                        |
| macOS?                                         | Supported on Apple silicon. The sandbox is best-effort and needs Apple `container`, podman or docker; `sealed` needs podman or docker because the `apple` backend refuses offline. The archive is unsigned: clear quarantine before first launch; Homebrew is unaffected. Set Option as Meta; `thegn doctor` names the setting.                                                                                                                         | `KNOWN_ISSUES.md`, `docs/help/known-limitations.md`                    |
| Windows?                                       | Deferred, no binary. WSL2 with the Linux x86_64 archive; the native MSVC port compiles but its first CI run failed in the named-pipe IPC tests.                                                                                                                                                                                                                                                                                                         | `KNOWN_ISSUES.md`                                                      |
| License? Business model?                       | `MIT OR Apache-2.0`. `[OWNER: one sentence, decided before posting.]`                                                                                                                                                                                                                                                                                                                                                                                   | `Cargo.toml`                                                           |
| Why alpha.132 already?                         | One release per batch of landed work; the cadence is not a feature-completeness signal.                                                                                                                                                                                                                                                                                                                                                                 | `CHANGELOG.md`                                                         |
| Databases or ports per worktree?               | Host-side lifecycle hooks and port sharing exist, both until configured; there is no per-worktree database isolation. Say so plainly.                                                                                                                                                                                                                                                                                                                   | `docs/help/environments.md`, `docs/help/share-and-forward.md`          |
| Did AI write it?                               | `[OWNER: decide the honest answer beforehand; commenters scan for AI-slop copy.]`                                                                                                                                                                                                                                                                                                                                                                       | —                                                                      |

Do-not-say list: "blazing", "seamless", "supercharge", "revolutionary",
"AI-powered", and any speed claim. `docs/performance-benchmark-protocol.md`
calls the sub-300 ms and sub-16 ms numbers targets, and
`docs/help/comparison.md` makes no performance comparison; speed adjectives
stay out unless the bench numbers were published (see Preconditions).

## Pinned start-here discussion

Enable Discussions with the categories Q&A, Ideas and Show and tell, post
this, pin it, and pin the launch release. `SUPPORT.md` already sends questions
to Discussions when they are enabled, so the categories and the pin must exist
before 06:00.

Title: **Start here: known issues, where to ask, and day-1 answers**

Body (paste as-is; the links are already absolute):

> Welcome. thegn is a terminal workspace that is also its own multiplexer:
> each git repo is a project, each git worktree is a tab, and panes are PTYs
> owned by a background daemon. This is a public alpha — Linux on x86_64 and
> macOS on Apple silicon are supported; Windows is deferred.
>
> Read these first:
>
> - Known issues: https://github.com/blakeashleyjr/thegn/blob/main/KNOWN_ISSUES.md
> - The honest limit list: https://github.com/blakeashleyjr/thegn/blob/main/docs/help/known-limitations.md
> - Install and the tour: https://github.com/blakeashleyjr/thegn#install
>
> Where to put what:
>
> - A question or an idea goes to this Discussions area (Q&A / Ideas).
> - A bug goes to a GitHub issue from the template, with `thegn doctor` output:
>   https://github.com/blakeashleyjr/thegn/issues/new/choose
> - Anything security-sensitive goes to the private channel in SECURITY.md,
>   never a public issue:
>   https://github.com/blakeashleyjr/thegn/blob/main/SECURITY.md
>
> Day-1 answers we already know:
>
> - Alt chords do nothing: your terminal must pass Alt through to the app; on
>   macOS set Option as Meta/Alt. `thegn doctor` names the setting.
> - A pane launch was refused: that is the sandbox refusing to run unsandboxed
>   — there is no host fallback. Install Apple `container`, podman or docker,
>   or read the sandbox backends page:
>   https://github.com/blakeashleyjr/thegn/blob/main/docs/help/sandbox-backends.md
> - The Mac tarball hangs on first launch: run
>   `xattr -dr com.apple.quarantine ./thegn` before launching it. Homebrew
>   installs are unaffected.
> - "Refusing database schema migration" after an upgrade: run
>   `thegn daemon stop` and relaunch — the old daemon still held the database.
>
> Show your setups in "Show and tell" — screenshots of the sidebar and panel
> are exactly what helps an alpha. The maintainer is in the launch thread
> today, so replies here may lag until the evening.

Maintenance rule: edit the top post in place and never pin a second copy. In
the evening, add a dated "day-1 recap" reply with whatever repeated in the
thread and link that reply from the top post.

## After the day

- Write down what worked and what broke while it is fresh; the kit's checklist
  owns the full retro, this page only owns the day.
- Fold the FAQ scratch file into the README or a FAQ page; move anything that
  repeated into KNOWN_ISSUES.md.
- Re-check the pinned discussion answers against anything a hotfix release
  changed, and update them in place.
- Tuesday: revisit the afternoon channels only if the thread is still moving;
  no HN resubmission the same week.
