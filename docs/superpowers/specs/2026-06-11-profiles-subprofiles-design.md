# Design: Profiles & Subprofiles

## Context

superzej today is **one process, one world**: a single global config
(`$XDG_CONFIG_HOME/superzej/config.toml`), a single SQLite DB
(`$XDG_STATE_HOME/superzej/superzej.db`), one set of credentials (whatever the
launching shell exported), one theme. The roadmap (`tasks.md` items 101–110)
already calls for **profiles** ("work/personal/etc.") with separate state dirs,
config/theme, credential isolation, sandbox/network policy, and a switcher —
and it flags profiles as _cross-cutting, seed early_. The Comms feature (items
467–480: email/matrix/irc/slack tiles + unified inbox + "comms/dev/personal"
presets) is the first subsystem that needs a finer split than a whole profile.

We want two levels of isolation:

- **Profile** — a complete firewall over the _entire_ program. Think "Work" vs
  "Personal": different storage, config, theme, credentials, git identity,
  sandbox/network policy. The current shell view is one profile.
- **Subprofile** — scopes _one subsystem_ inside a profile, inheriting
  everything else. Motivating case: keep all _development_ in one profile
  (unified dev), but split _Comms_ into work vs personal accounts/storage —
  without needing two whole profiles.

**This document is design-only.** It defines the model, storage layout, config
schema, firewall mechanism, process/window model, and how Comms consumes it.
No implementation steps are prescribed yet; an implementation plan would follow
from this. Tracked as roadmap items H. 101–110, 536–539 and AM. 540.

Decisions locked with the user:

1. **Profile = separate OS process + separate scope root.** Multiple windows run
   concurrently, one per profile. Subprofiles switch **in-process**.
2. **Shared base config + per-profile overrides** (not fully independent).
3. **Firewall everything**: state/DB, config/theme, credentials + git identity,
   sandbox backend + network policy.

---

## Core model

Three concepts, one of which (Subsystem) is the organizing spine:

- **Subsystem** — a coherent feature area with its own storage and identity.
  `workspace` (the existing IDE shell: sidebar/worktrees/panel/pins/center),
  `comms` (new), later `ai`. Each subsystem declares whether it supports
  subprofiles and how to root its storage.
- **Profile** — a firewalled container of _all_ subsystems, realized as its own
  process rooted at a profile scope. `work`, `personal`, … plus a `default`
  profile that preserves today's behavior.
- **Subprofile** — an alternate identity for _one_ subsystem inside a profile.
  Default subprofile = the profile-level storage (shared). Named subprofiles
  (e.g. comms `work` / `personal`) get isolated storage + credentials for that
  subsystem only.

The current shell _is_ the `workspace` subsystem. A profile in the user's
example: **one profile**, `workspace` on its default subprofile (unified dev),
`comms` split into `work`/`personal` subprofiles.

```
Profile "default"
├── workspace   → subprofile "default"        (unified dev)
└── comms       → subprofiles "work","personal" (split)
```

---

## The firewall mechanism — reroot the environment at startup

**Key insight: the codebase is already env-driven.** `sandbox::resolve` reads
`env_passthrough` from `std::env::var` (`crates/superzej-core/src/sandbox.rs:255`),
`gh::resolve_token` reads `GH_TOKEN`/`GITHUB_TOKEN` from the process env
(`crates/superzej-core/src/gh.rs:75`), and the three path roots —
`xdg_config_home()`, `xdg_state_home()`, `superzej_dir()`
(`crates/superzej-core/src/util.rs:16-37`) — read env on every call. So the
firewall should be enforced by **setting a profile-scoped process environment
once, as the very first thing in `main`**, _before_ any thread spawns or any
DB/config opens. Then paths, sandbox env-passthrough, and forge token resolution
all become correct _for free and consistently_.

Threading a `&ProfilePaths`/cred struct through call sites is the wrong choice
here: it would touch the ~20 `Db::open()` sites in `run.rs` and still leave the
`std::env::var` reads in `sandbox.rs`/`gh.rs` leaking the launching shell's
global creds.

### Startup sequence (in `crates/superzej-host/src/main.rs`, before tokio/threads)

1. Resolve the active profile from `--profile` / `SUPERZEJ_PROFILE` (default
   `"default"`).
2. Compute the profile roots and `std::env::set_var` them:
   - `SUPERZEJ_DIR = <sz_state>/profiles/<p>` (covers `~/.superzej` derivatives:
     activity, audit, run/ sockets, yazi).
   - `XDG_STATE_HOME = <sz_state>/profiles/<p>/state` (covers DB + logs).
   - Credential vars (see Firewall §): `GIT_CONFIG_GLOBAL`, `GH_CONFIG_DIR`,
     `GH_TOKEN`, `GIT_SSH_COMMAND`, `GNUPGHOME`.
3. **Do NOT blanket-reroot `XDG_CONFIG_HOME`** — see config layering below; the
   _shared base_ config must still load from the real config home.
4. Initialize a write-once `ProfilePaths` (an `OnceLock<ProfilePaths>` in
   `util.rs`) as a typed accessor over the now-rerooted env, for in-process
   clarity.

> ⚠ `std::env::set_var` is `unsafe`/not thread-safe under Rust 2024. It must run
> as the first statement in `main`, before the tokio runtime and before any PTY
> reader thread (`pane.rs:129`). Sequencing is load-bearing for correctness: a
> single `Db::open()` before rerooting would touch the wrong DB.

`util.rs` path helpers and `db_path()` keep their signatures; they read the
rerooted env. `Db::open_at()`/`open_memory()` stay independent so tests remain
isolated.

---

## Storage layout

```
$XDG_STATE_HOME/superzej/                    # real root (unrooted)
  profiles/
    default/                                 # legacy data migrates here
      state/superzej.db                       # per-profile DB (WAL, schema v6+)
      state/logs/
      activity.json  audit.log
      run/            szhost.lock, ssh-* sockets   # per-profile, never shared
      config/git/config        # profile git identity (user.name/email, signing)
      config/gh/               # GH_CONFIG_DIR (profile token)
      ssh/                     # profile SSH key(s)
      gnupg/                   # GNUPGHOME
      comms/                   # comms subsystem root
        <subprofile>.db        # e.g. work.db / personal.db (own handle)
        <subprofile>/          # maildir, chat caches, attachments
    work/   …
    personal/   …

$XDG_CONFIG_HOME/superzej/
  config.toml                                # SHARED BASE (unrooted)
  profiles/<p>/config.toml                   # profile override layer
  profiles/<p>/comms/<subprofile>.toml       # subprofile (subsystem) override
```

### Worktrees must NOT move

`~/.superzej/worktrees/` and absolute worktree paths are baked into git's
`.git` gitdir pointers and stored absolutely in the `worktrees`/`tab_groups`
tables (and relied on by sandbox path-preservation, `sandbox.rs:5-11`). Migration
**reroots DB/logs/activity/audit/sockets but leaves existing worktrees in place**.
Going forward, `worktrees_dir` becomes a per-profile config value; existing
absolute paths are grandfathered.

### Migration (first launch, `default` profile)

Detect "legacy layout present, no `profiles/` dir" → move DB, logs,
activity.json, audit.log under `profiles/default/` (leave worktrees). Existing
single-profile users see no behavioral change.

---

## Config layering

`Config::load_layered` (`crates/superzej-core/src/config.rs:1438`) gains a
profile layer. **Two config roots** (this is the reason `XDG_CONFIG_HOME` is not
blanket-rerooted):

Precedence (low → high):

1. Built-in defaults (`Config::default()`).
2. **Shared base** `config.toml` from the _real_ `XDG_CONFIG_HOME` (loaded
   regardless of active profile).
3. **Profile override** `profiles/<p>/config.toml` (theme, settings, sandbox
   defaults, network policy, keybinds — a _full_ overlay).
4. **Subprofile override** `profiles/<p>/<subsystem>/<subprofile>.toml` —
   applies _only_ to that subsystem's section.
5. Per-workspace overlay (`[workspace.<slug>]`) → repo-root `.superzej.*` →
   `SUPERZEJ_*` env → `--set` (unchanged existing layers).

### `ProfileConfig` must grow from keybinds-only to a full overlay

Today `ProfileConfig` (`config.rs:383`) holds only `default_mode` + `keybinds`.
The design promotes a profile to a full config overlay (theme/palette, sandbox,
network, credentials pointers, comms). The cleanest realization given decision 1
(profile = separate process) is to read the profile overlay from the
**per-profile config file** rather than nested `[profiles.<name>]` tables — one
file per profile is far more legible than deep nesting and matches the
firewalled-process model. The existing inline `[profiles.<name>].keybinds` can
remain supported for back-compat (keybind-only profiles), but full profiles live
in their own file.

---

## Credential + git-identity firewall

Two structural leaks exist today and must be closed:

1. **`pane.rs::spawn_with_env` inherits the entire parent env** and only _adds_
   `TERM`/`COLORTERM`/passed vars on top (`pane.rs:108-119`). A raw shell pane
   therefore sees every var szhost inherited — including any `GH_TOKEN`,
   `SSH_AUTH_SOCK`, `ANTHROPIC_API_KEY` the launching shell exported.
   **Fix: clear-then-allowlist.** Start from a curated base (`PATH`, `HOME`,
   `TERM`, `LANG`, `USER`, …) and layer profile creds on top, instead of
   inherit-everything.
2. **The sandbox env path bypasses the pane env.** `sandbox::resolve` re-reads
   `env_passthrough` from the _process_ env (`sandbox.rs:255`) and injects via
   `-e`/`--setenv`. Because we reroot the process env at startup (Firewall §),
   this path now injects the _profile_ creds automatically — but only if
   rerooting is wired. Also: the default sandbox mount `~/.gitconfig:ro`
   (`config.rs:853`) mounts the user's **global** identity into every container;
   it must point at `profiles/<p>/config/git/config`.

### Profile-scoped credential env (set at startup, inherited by all panes/sandboxes)

- `GIT_CONFIG_GLOBAL = profiles/<p>/config/git/config` — profile `user.name`,
  `user.email`, signing key. (Note: does not cover `GIT_CONFIG_SYSTEM`
  `/etc/gitconfig`; credential helpers in the shared base gitconfig can leak
  tokens cross-profile — audit these.)
- `GH_CONFIG_DIR = profiles/<p>/config/gh`, `GH_TOKEN = <profile token>`. The
  host side (`gh::resolve_token`) then resolves the profile token too.
- `GIT_SSH_COMMAND = ssh -i profiles/<p>/ssh/id -o IdentitiesOnly=yes` — the
  `IdentitiesOnly=yes` is essential or ssh offers the agent's other keys and the
  firewall leaks. Optionally a per-profile `SSH_AUTH_SOCK`.
- `GNUPGHOME = profiles/<p>/gnupg`.

### Sandbox caveats to document

`file_access = all` / `--dev-bind / /` (`sandbox.rs:756`) mounts the whole host
RW, defeating any cred firewall (the other profile's `profiles/` tree is
reachable). `--network host` shares localhost brokers. SSH agent forwarding
(`-A`) exposes all agent keys. The firewall is only as strong as the loosest
sandbox/file-access setting — document this prominently.

Per-profile **sandbox backend defaults** and **network policy** are just fields
in the profile config overlay, consumed by the existing `Config::repo_sandbox`
seam — no new mechanism needed.

---

## Process & window model

Profile = separate process. Concretely on Linux:

- **Singleton per profile via advisory `flock`** on
  `profiles/<p>/run/szhost.lock` (`LOCK_EX | LOCK_NB`), held for process
  lifetime. The kernel releases it on death (incl. SIGKILL) — no stale-pidfile
  cleanup. One-shot non-blocking check at startup (never a poll loop — that would
  break the ~0%-idle invariant). Write pid + window marker into the file for the
  focus path. Per-profile DB means cross-profile WAL is contention-free.
- **Switching/launching a profile window** — szhost owns a raw TTY and has no
  window of its own, so there is no portable "focus my window" primitive:
  - _Primary (works everywhere incl. Wayland/SSH):_ spawn a new terminal
    emulator running `szhost --profile=X` via a per-profile, configurable launch
    command (`$TERMINAL -e …`).
  - _Best-effort focus (X11 only):_ stamp a unique per-profile marker via OSC
    title (`util.rs:187 set_terminal_title`) and match with `wmctrl`/`xdotool`;
    fall back to spawn. **Wayland cannot focus foreign windows** — spec spawn as
    the primary, focus as an optimization only.
  - _Distinct action "switch this window":_ re-exec in place
    (`util.rs:208 exec_command`) — single-window, explicitly different from
    "open in a new window."

The existing in-process `live_instance` (`pins.rs:243`) is **not** a
cross-process guard; the `flock` is the real one.

---

## Subprofiles (in-process) & the Subsystem boundary

A subprofile switch (comms work↔personal) must re-scope _one_ subsystem without
touching `workspace`. There is no central `App` struct today — `event_loop`
(`run.rs:~2300`) owns everything as locals. Introduce a small `Subsystems`
holder (or a minimal `App`) in the loop's locals.

Define a `Subsystem` trait whose instances own:

- their **storage handle** (comms → its own DB file `comms/<subprofile>.db`, so
  teardown is a clean handle drop, not closing the shared profile DB),
- their **credential scope** (which accounts/tokens — assembled through the same
  `launch_spec` env seam),
- their **pane set** (pane _ids_, asking the loop's `Panes` to kill them rather
  than owning `&mut Panes` — avoids borrow-checker fights with the loop).

Switch = `subsystem.teardown()` (kill its panes, drop its DB handle/cred scope)
→ `subsystem.bind(new_scope)`. `workspace` is untouched by construction.

**Do not assume the "cache over git" model.** `workspace` state is largely
re-derivable from git (`db.rs:1-10`: DB is a cache/resurrect layer). **Comms is
the opposite — its store is authoritative** (mail/chat is not in git). The
`Subsystem` trait must make storage ownership explicit per subsystem, not bake in
rederive-from-git.

**0%-idle invariant:** `bind()` must do no polling; comms periodic work (e.g.
mail fetch) must ride the existing `TerminalWaker` (`panes.rs:209`) — never a
timer in the main loop.

---

## Comms — the first subprofile consumer (design sketch)

- A `comms` subsystem with an **account registry** (email/chat) stored under
  `profiles/<p>/comms/<subprofile>/`, its own `comms/<subprofile>.db`, and creds
  drawn from the profile credential store partitioned by subprofile.
- Subprofiles `work` / `personal` give isolated account sets + storage while the
  surrounding profile (and the `workspace` dev subsystem) stays unified — exactly
  the user's case.
- UI: an in-subsystem subprofile switcher (work↔personal) distinct from the
  top-level profile switcher. All comms pane spawns route through the shared
  `launch_spec` env seam so they inherit the right credentials.
- Aligns with roadmap items 467–480 (tiles, unified inbox, comms/dev/personal
  presets).

---

## Critical files (where the design lands)

- `crates/superzej-host/src/main.rs` — startup env-rerooting (first statements),
  `--profile` flag, migration trigger, `flock` singleton, profile-launch action.
- `crates/superzej-core/src/util.rs` — `ProfilePaths` (`OnceLock`) typed
  accessor; path helpers read rerooted env.
- `crates/superzej-core/src/config.rs` — two-root layering (shared base +
  profile + subprofile); promote `ProfileConfig` to a full overlay.
- `crates/superzej-host/src/agent.rs` — `launch_spec` assembles profile/subprofile
  credential env (shared seam for agent + shell panes).
- `crates/superzej-host/src/pane.rs` — `spawn_with_env` clear-then-allowlist.
- `crates/superzej-core/src/sandbox.rs` — fix `~/.gitconfig` mount → profile
  gitconfig; verify env-passthrough picks up rerooted creds; document
  file_access/network leaks.
- `crates/superzej-core/src/gh.rs` — token resolution honors `GH_CONFIG_DIR`/
  profile token.
- `crates/superzej-core/src/db.rs` — per-profile DB path via rerooted env;
  comms gets its own DB file.
- `crates/superzej-host/src/run.rs` — `Subsystems` holder; subprofile
  bind/teardown wiring in the event loop.

---

## Verification (how this would be validated)

Design-only, so verification is specified for the eventual implementation:

1. **Path/firewall unit tests** (core, gated 95%): given `SUPERZEJ_PROFILE=work`,
   assert DB/logs/activity/sockets resolve under `profiles/work/`; assert shared
   base config still loads from the real `XDG_CONFIG_HOME`; assert `default`
   profile + legacy layout triggers migration and leaves worktrees in place.
2. **Singleton test**: second `flock` on a live profile fails non-blocking;
   no busy-poll (assert no CPU spin).
3. **Cred-leak test**: spawn a shell pane, dump env, assert no
   `GH_TOKEN`/`SSH_AUTH_SOCK`/API keys from the launching shell survive; assert
   `GIT_CONFIG_GLOBAL`/`GH_CONFIG_DIR`/`GIT_SSH_COMMAND` point at the profile;
   `git config user.email` inside a pane returns the profile identity.
4. **Concurrency smoke** (`test/smoke.sh`-style, isolated XDG): launch two
   profiles concurrently, write in each, assert separate DBs and zero
   cross-profile state bleed.
5. **Subprofile switch**: in comms, switch work↔personal; assert comms panes/DB
   handle rebind while `workspace` panes/state are untouched; assert no idle
   wakeups introduced (`SUPERZEJ_LOG` waterfall / idle-CPU check).
