# THE-77 — Architecture soundness audit: design

Audit of the working tree at `tg/the-77-arch-audit` (base `982ab7cb`) against
`docs/ARCHITECTURE.md` §1–§10 and the CLAUDE.md hard invariants, per the issue's
10-point checklist.

**Headline: the architecture is in good shape. The drift is in the _gates_, not
the code.** Nine of the ten checklist items pass on the code side. What this
audit found instead is three places where an invariant ARCHITECTURE.md claims is
enforced is, in fact, not enforced — a broken comparison in the shared ratchet
driver, a substrate with no owner row, and a whole crate that never opted into
the workspace lint block. ARCHITECTURE.md's own thesis is _"an invariant with no
gate is a wish, not an architecture"_; these findings are wishes wearing a gate's
clothes, which is worse than a known gap because `just lint` prints green.

## Method

Every claim below was produced by running the gate or reading the cited code, not
by inference. The bash ratchets were executed directly (they are grep-only and
cost nothing); the Rust-side ratchets, catalog coverage tests and boundary tests
were read rather than run — this turn's budget forbids a full-workspace compile,
and each finding is stated so a coder's scoped `cargo nextest run -p <crate>` run
confirms or refutes it in one shot.

## Checklist verdict

| #   | Item                                                         | Verdict                                                                   |
| --- | ------------------------------------------------------------ | ------------------------------------------------------------------------- |
| 1   | 0% idle / no blocking I/O on the loop or before first frame  | **PASS with one documented carve-out** — see F5                           |
| 2   | `render_plan::plan` is the only render decision              | **PASS** — exactly one call site, `run.rs:11599`                          |
| 3   | color/glyph literals only at the chokepoints                 | **PASS** — both ratchets clean (10 / 32 pinned, all with written reasons) |
| 4   | `thegn-core` substrate-free                                  | **PASS on core; gate has a hole** — see F2                                |
| 5   | vendor CLIs only in seam impls; no `async fn` in seam traits | **PASS** (one exception → follow-up, see F7)                              |
| 6   | every external door projects `capability::CATALOG`           | **PASS** — no unprojected verb found                                      |
| 7   | DB writes are cache/resurrection only                        | **PASS** — exemplary; see below                                           |
| 8   | every long-lived thread declares a `platform::qos` class     | **FAIL** — see F4                                                         |
| 9   | `let _` / `.ok()` on primary paths justified                 | **PASS on the ratchet; gate has a hole** — see F3                         |
| 10  | ratchet entries fixable in < 1 chunk                         | **One found** — see F6                                                    |

### What passed, and the evidence

**(1) 0% idle.** `just lint`'s guardrail (`justfile:593-594`) reproduces clean:
no timed `poll_input` outside `idle_poll::poll_timeout`, exactly one
`poll_input(timeout)` site (`run.rs:12418`). Every other site is `None` or a
`Duration::ZERO` drain. Better than the doc claims, `crates/thegn-host/clippy.toml:24-29`
bans `Command::output` / `status` / `Child::wait` outright in the host, and every
legitimate site carries an `#[expect]` with a one-line justification naming _why_
it is off-loop (e.g. `run.rs:2762` "off-loop: inside spawn_blocking",
`sandbox_events.rs:64` "runs on the events-stream thread"). Off-thread producers
pulse the waker at the send (`run.rs:2783`, `run.rs:2117`).

**(2) Render decision.** `crate::render_plan::plan` is called from exactly one
place, `run.rs:11599`. Nothing else decides.

**(3) Degradation chokepoints.** `test/color-literal-ratchet.txt` (10) and
`test/glyph-literal-ratchet.txt` (32) both pass, and — notably — the entries are
annotated with _why_ (relayed SGR in `diff_view.rs`, decorative art in
`logotype.rs`), which is the standard the other ratchets should be held to.

**(6) Capability catalog.** `API_CALLS` is pinned against `ROUTES`
(`thegn-svc/src/control/routes.rs:308-311`), `SURFACE_GAPS` is pinned
shrink-only by `test/surface-gaps-ratchet.txt` with the reason column
regenerated from the table so the two cannot drift, and `coverage_problems`
(`capability.rs:1188`) reports all three drift directions including a stale
excuse. I could not construct a surface that grew a verb without a catalog row:
each surface's implemented set is a table the arbiter reads, so the failure mode
the checklist asks about is structurally closed.

**(7) DB writes.** `crates/thegn-host/src/db_task.rs:1-22` is the model answer:
a dedicated writer thread, best-effort cache scope stated in the module doc, the
session-layout persist explicitly flushed before exit and before a cold
workspace-switch resurrect, and — the part that matters for §2 — it blocks on
`recv()` with no timer, so it adds zero idle wakes. No unrouted synchronous
`Db::open()` on a loop path was found.

## Findings

Severity: **HIGH** = a claimed gate does not fire; **MED** = an invariant with no
gate is drifting; **LOW** = pinned debt burnable now.

---

### F1 — HIGH — `test/ratchet.sh` compares locale-sorted input with a byte-wise `comm`

`test/ratchet.sh:32` and `:49` build both sides with a bare `sort -u`, which uses
`LC_COLLATE` (here `en_US.UTF-8`, where `.` and `_` are ignored at the primary
level). `test/ratchet.sh:53-54` then feeds those lists to `comm`, which requires
byte order. The two disagree, and `comm` says so on every run:

```
$ bash test/ratchet.sh ignored-result 'let _ = |…' crates
comm: file 1 is not in sorted order
comm: file 2 is not in sorted order
ratchet(ignored-result): clean (323 pinned)
```

This is not cosmetic. Brute-forcing the 323-entry `ignored-result` list:

- removing any one of **19 of 323** pinned entries makes the ratchet report an
  _unrelated, still-violating_ file as a stale entry — the message tells the
  maintainer to delete a line that is load-bearing;
- of 800 synthetic "a new violating file appears" scenarios, **213** produce a
  spurious stale-entry error alongside the real one. Example: a new violation in
  `crates/thegn-core/src/config_resolvezz.rs` reports
  `crates/thegn-core/src/config.rs` as stale.

The failure is loud rather than silent (no missed violation was found in the
probes), so nothing has slipped through yet — but the gate's contract is
"the list only shrinks", and it currently emits instructions to shrink it
_wrongly_. Five of the seven bash-driven ratchets, including the 323-entry
ignored-result list ARCHITECTURE.md §9 names as its gate, run through this path.

Fix: force `LC_ALL=C` on the `sort`s and the `comm`s.

_Note the Rust twin is unaffected_ — `file_ratchet`
(`thegn-core/src/test_support/ratchet.rs:139-185`) uses `BTreeSet`, which is
byte-ordered by construction.

---

### F2 — HIGH — `gix` is a substrate with no owner row, so nothing pins it to `thegn-svc`

ARCHITECTURE.md §1 lists the substrates as "tokio, termwiz, portable-pty,
reqwest, octocrab, axum, alacritty_terminal, gix" and states "each substrate has
exactly the owner crates listed in `crates/thegn-core/tests/crate_boundaries.rs`".

`OWNERS` (`crate_boundaries.rs:28-50`) has rows for seven of those eight. `gix`
appears only in `CORE_FORBIDDEN` (`:66`), which — per that constant's own doc
comment at `:52-54` — bans it _from `thegn-core`_ and nowhere else. So
`substrates_are_only_used_by_their_owners` (`:112`) never considers `gix`, and
`thegn-host` could grow a direct `gix` dependency with every gate green. Today
only `thegn-svc` declares it (`crates/thegn-svc/Cargo.toml:37`), so this is a
zero-diff fix that makes a true statement enforced.

Fix: add `("gix", &["thegn-svc"])` to `OWNERS`.

---

### F3 — HIGH — `thegn-proxy` never opted into `[workspace.lints]`, so `let_underscore_future = deny` is inert there

ARCHITECTURE.md §9 names `let_underscore_future = deny` as one of the three gates
on the ignored-`Result` invariant. It is declared at `Cargo.toml:241`. Cargo
applies `[workspace.lints]` **only** to members that declare `[lints] workspace =
true` in their own manifest. Eleven of the twelve workspace members do.
`crates/thegn-proxy/Cargo.toml` does not — it has no `[lints]` section at all.

`thegn-proxy` is exactly the crate where this matters: it is the async I/O shell
(tokio + axum + reqwest), the one member whose whole body is futures. Its current
`let _ =` sites are all `.await`ed (`relay.rs:338,343`, `lib.rs:72,83`,
`router.rs:290,466`), so **no dropped future exists today** — the finding is that
the tripwire is disarmed, not that it has been tripped.

The same omission also disarms `unexpected_cfgs` / `check-cfg(kani)` for that
crate; harmless today (no `cfg(kani)` in proxy) but the same class.

Fix: add `[lints] workspace = true` to the proxy manifest, **and** pin the
invariant so member #13 cannot repeat it — `crate_boundaries.rs` already parses
every member manifest (`members()`, `:74-105`), so the assertion is a few lines
in the file that already owns "a new crate must be placed".

---

### F4 — MED — long-lived background threads do not declare a QoS class, including the three the doc names as its examples

CLAUDE.md: _"New long-lived threads should declare a class; the default is
Interactive, which for background work is wrong."_ `platform/qos.rs:1-27` names
its motivating cases explicitly: _"background hydration, metrics polling,
fs-watching and git fan-out all compete for P-cores with the render loop"_.

Fourteen call sites declare a class today (`hydrate.rs` ×4, `monitor.rs:798`,
`monitor_action.rs` ×2, `repo_index.rs:248`, `push_notify.rs:40`,
`model_proxy_daemon.rs:222`, `handlers/startup.rs:116`,
`handlers/paste_image.rs:71`, `run.rs:391` (the loop, `Interactive`),
`run.rs:6605`). Roughly thirty named, long-lived `thread::Builder` threads do
not. Two of the doc's own three examples are among them:

| Site                                         | Thread               | What it is                                                                                                 | Should be    |
| -------------------------------------------- | -------------------- | ---------------------------------------------------------------------------------------------------------- | ------------ |
| `crates/thegn-host/src/metrics.rs:78`        | `thegn-metrics`      | the metrics sampler supervisor — sleeps `interval_secs` between blocking HTTP scrapes, nothing waits on it | `Background` |
| `crates/thegn-host/src/bridge_sup.rs:123`    | `bridge-fswatch`     | fs-watch event pump, `while rx.recv().is_ok()` for the client's lifetime                                   | `Background` |
| `crates/thegn-host/src/sandbox_events.rs:46` | `podman-exec-events` | blocks on `podman events` stdout for the process lifetime, writes audit rows                               | `Background` |
| `crates/thegn-host/src/sandbox_events.rs:54` | `podman-net-events`  | same, network events                                                                                       | `Background` |

Deliberately **not** in the fix set, and why — this is the part a mechanical
sweep gets wrong:

- `loading/ticker.rs:69` (`thegn-splash-tick`) drives an animation the user is
  actively watching. `Background` on Apple silicon would visibly stutter it.
- `db_task.rs:54` (`thegn-db-writer`) is synchronously awaited by the loop at
  `flush()` before a cold workspace-switch resurrect and on clean exit.
  Demoting it can stall the loop.
- `metrics.rs:229` (`thegn-metrics-collect`) is `recv_timeout`-ed by its parent;
  a demotion interacts with that deadline.
- `frame_writer.rs:100` (`thegn-writer`) is on the render hot path — if it
  declares anything it is `Interactive`, which is already the default, so
  touching it is churn.

Those four belong in the allowlist with the reason written down, not in a sweep.

The deeper finding is that **this invariant has no gate at all** — which is why
it drifted in exactly the places its own documentation names. It should get one,
in the established shape (`file_ratchet` + a shrink-only `test/*.txt`), so the
remaining ~30 sites become written debt rather than invisible debt.

---

### F5 — MED — ARCHITECTURE.md §2 and `crates/thegn-host/clippy.toml` disagree about blocking work before the first frame

`docs/ARCHITECTURE.md:62-65`: _"Never put blocking I/O on the loop — including at
startup: anything before the first frame that can block … runs on a thread under
a cap."_ `crates/thegn-host/clippy.toml:18-19` sanctions the opposite: _"every
legitimate site (CLI subcommands in src/cmd/, **startup before the loop**, …)
carries a local `#[expect]`."_

The code follows clippy.toml. `run.rs:592-608` runs, on the loop thread, before
the first frame (which is logged at `run.rs:12355`):

- `heal_main_checkout_worktree(&cwd)` and one call per session worktree group
  (`run.rs:592-595`). Linked worktrees bail on a `stat` (`util.rs:360`), so the
  per-worktree cost is real but small;
- an unconditional `git rev-parse --path-format=absolute --git-common-dir`
  (`run.rs:602-605`, `#[expect]`ed "startup: runs once before the event loop
  exists");
- `heal_main_checkout_worktree(&common_parent)` (`run.rs:608`), which for a main
  checkout runs 2–5 further blocking git subprocesses via `git_out`/`git_ok`
  (`util.rs:578-606`) before it can decide there is nothing to heal.

So a launch pays 1–6 sequential `git` process spawns on the critical path to the
first frame, against a 300 ms budget. That is a deliberate, reasoned trade (the
comment at `run.rs:588-591` explains why the heal must happen before anything
reads git), not an accident — but ARCHITECTURE.md currently states a rule the
repo does not follow, and a reader reconciling the two has no way to tell which
is authoritative.

**No code change proposed.** Moving the heal off-loop changes when the repair
lands relative to every subsequent git read, which is a behavioural change, not
an audit fix. Filed as a follow-up (P1 below) so the measurement and the
decision happen together.

---

### F6 — LOW — `test/help-context-ratchet.txt` is two entries from empty, and both are stubs

`panel:db` and `panel:debug` are the entire remaining debt. Both sections are
inert placeholders outside `SECTION_ORDER`:
`panel/sections/misc.rs:1160` renders _"db introspection not wired yet"_,
`:784` renders _"debugger integration not wired yet"_. They are reachable via
`help::context::vocabulary()` (`help/context.rs:50-55`, which chains them in
explicitly) and today F1 in either lands on the generic index page
(asserted at `help/pages.rs:200`).

The honest burn-down is not to document them as features but to document them as
reserved — the same move §6 already makes for routed-but-inert capability rows
(_"a routed-but-inert row like `browser.drive` carries a `stub` marker so it
never reads as done"_). A short "Reserved sections" block in `docs/help/panel.md`
claiming both contexts takes the file to zero and makes F1 truthful in both.

---

### F7 — INFO — `sandbox_events.rs` hard-codes `podman` outside the sandbox seam

§5 says vendor CLIs are called only inside their implementation files.
`crates/thegn-host/src/sandbox_events.rs:68,139` calls `Command::new("podman")`
directly from the host, gated on `util::have("podman")`, with no
`sandbox::Backend` involvement and no ratchet covering it (the `forge-leak`
ratchet only greps for `gh`).

This is defensible in substance — `podman events --format json` has no docker
equivalent with the same shape, so the audit subscriber genuinely is
podman-specific — but it is unpinned, which means the next `docker`/`bwrap`
special case has no gate to stop it. Too large for this change (it needs an
optional `events()` op on the sandbox seam with a `caps()` bit). Filed as P2.

---

## Chunks

Three chunks. **Chunk 1 and Chunk 2 and Chunk 3 are fully file-disjoint and have
no ordering dependency — all three can run in parallel.** Chunk 1 touches
`test/ratchet.sh`, `crate_boundaries.rs`, `crates/thegn-proxy/Cargo.toml`;
Chunk 2 touches `crates/thegn-host/src/{metrics,bridge_sup,sandbox_events,platform_ratchet_tests}.rs`
plus a new `test/thread-qos-ratchet.txt` and the `justfile`; Chunk 3 touches
`docs/help/panel.md`, `crates/thegn-host/src/help/pages.rs` and
`test/help-context-ratchet.txt`.

The one shared file to watch: **Chunk 1 edits `test/ratchet.sh`, Chunk 2 adds a
line to the `justfile`'s `ratchet-update` recipe.** Different files — no conflict.

| Chunk | Findings   | Files                                                                                             | Risk                                                                                                         |
| ----- | ---------- | ------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| 1     | F1, F2, F3 | `test/ratchet.sh`, `crates/thegn-core/tests/crate_boundaries.rs`, `crates/thegn-proxy/Cargo.toml` | low — two of three are one-liners; the proxy lint opt-in may surface warnings, which is the point            |
| 2     | F4         | 3 host source files, 1 host test file, new `test/thread-qos-ratchet.txt`, `justfile`              | low — `qos::set_self` is a no-op off macOS; the fix set deliberately excludes every thread on a latency path |
| 3     | F6         | `docs/help/panel.md`, `crates/thegn-host/src/help/pages.rs`, `test/help-context-ratchet.txt`      | low — docs + one test assertion                                                                              |

Full specs: `.thegn/pipeline/THE-77/code/chunk-{1,2,3}.md`.

## Explicitly not changed

Per the issue's "do not churn code that already conforms":

- No god-file refactoring. `run.rs` is untouched by all three chunks.
- The `ignored-result` (323), `env-overlay` (442), `completion-slot` (159) and
  `surface-gaps` (86) allowlists are left alone. Each is a real burn-down, none
  is a single chunk, and F1's fix must land first — auditing entries against a
  comparator that mis-reports staleness would waste the effort.
- The color (10) and glyph (32) allowlists are left alone: every entry already
  carries a written reason, several are correct-by-design (relayed SGR,
  decorative art), and burning the rest is a rendering change, not an audit fix.

## Proposed follow-up issues (for the Lead to file)

**P1 — Reconcile the "no blocking I/O before the first frame" rule with the
startup git heal (F5).** Measure `run.rs:592-608` on a cold launch with a
realistic worktree count; then either move the heal off-loop behind a barrier the
first git-reading consumer awaits, or amend ARCHITECTURE.md §2 to state the
carve-out that `crates/thegn-host/clippy.toml` already sanctions. Doing neither
leaves the single source of truth saying something untrue.

**P2 — Put `podman events` behind the sandbox seam (F7).** Add an optional
`events()` op with a `caps()` bit so `sandbox_events.rs` stops naming a vendor
binary in the host, and extend a ratchet to cover container-runtime CLI names the
way `forge-leak` covers `gh`.

**P3 — Burn down `test/thread-qos-ratchet.txt` (F4 remainder).** ~30 pinned
sites. Each needs a per-thread judgement about latency coupling (see the four
worked examples in F4), so it is a considered sweep, not a mechanical one.

**P4 — Audit the 323-entry `ignored-result` allowlist for checklist item 9.**
The ratchet proves the list only shrinks; it does not prove each `let _ =` is on
a best-effort path or carries the `// best-effort:` comment CLAUDE.md requires.
Sampling that at file granularity across `crates/` is its own change. Sequence
after Chunk 1 (F1).

**P5 — Grow `panel.md` to cover every panel section.** `help/pages.rs:194-197`
already records this: `page_for_context` is _"a reachability guarantee, not a
coverage one"_. Chunk 3 empties the ratchet, which removes the tripwire that
would otherwise have tracked it — so the coverage work needs its own issue.
