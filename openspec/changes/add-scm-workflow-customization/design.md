# Design — SCM workflow customization

## 1. Audit of the current source-control setup (the THE-30 deliverable)

What exists today, verified against source:

| Area                              | Today                                                                                                                                                                                                                                                                                                                                                                            | File anchors                                                                                                                         |
| --------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| Read seam                         | `GitBackend` (thegn-svc, sync, no caps struct — the one seam without one): 8 required + 16 defaulted methods; `GixGit` native for exactly 4 local reads (`is_dirty`, `current_branch`, `branches`, `ahead_behind`), everything else `CliGit`                                                                                                                                     | `crates/thegn-svc/src/git/mod.rs:236,516,1215`                                                                                       |
| Writes                            | Always the git CLI, via `util::git_cmd`/`GitLoc` (env-scrubbed, `GIT_OPTIONAL_LOCKS=0`), serialized by the `thegn-git.lock` flock; reads bounded by `THEGN_GIT_READ_TIMEOUT_SECS`, writes deliberately unbounded                                                                                                                                                                 | `crates/thegn-core/src/util.rs:314`, `crates/thegn-core/src/remote.rs:391`                                                           |
| Land flow                         | `attempt_land` → `fold::fold` → `merge-tree --write-tree` + `commit-tree -p tip -p branch` + `update-ref` CAS (≤5 retries); message hard-coded `Merge branch '<b>' (fold-actor)`; each branch folds against the running tip                                                                                                                                                      | `crates/thegn-host/src/integrate.rs:1036`, `crates/thegn-core/src/fold.rs:85,109`, `crates/thegn-svc/src/git/plumbing.rs:69,120,135` |
| Real `git merge` in the land path | only `regenerate_merge`: throwaway detached worktree, `merge --no-commit --no-ff`, lockfile regeneration, `write-tree` fed back to the fold                                                                                                                                                                                                                                      | `crates/thegn-host/src/integrate.rs:88`                                                                                              |
| Signing                           | `[git] override_gpg` (rebase/amend/cherry-pick only), per-commit `^S` inherit→sign→no-sign on the panel commit overlay (Y 328); **fold commits never signed** (`commit-tree` skips signing by design); **`snapshot_worktree` inherits ambient `commit.gpgSign` with no override**; no SSH-format handling anywhere (none needed — `--gpg-sign` defers format to user git config) | `crates/thegn-svc/src/git/mod.rs:1057`, `commit.rs:33`, `plumbing.rs:120,153`                                                        |
| Merge drivers / rerere            | zero hits; stageable diffs actively pin `--no-ext-diff --no-renames -c diff.noprefix=false` (`SANITIZED_DIFF`) so `git apply` round-trips                                                                                                                                                                                                                                        | `crates/thegn-svc/src/git/mod.rs:42`                                                                                                 |
| Config                            | `[git]`: `backend`, `override_gpg`, `merge_guard`, `auto_fetch{,_interval_secs,_min_interval_secs,_notify}`. Per-workspace overlays exist for `merge_queue`/`pr_queue` but **not** `[git]`; the doc comment on `MergeQueueOverlay` promises an untrusted `.thegn.*` `[merge_queue]` layer that `RepoConfigFile` does not actually carry                                          | `crates/thegn-core/src/config.rs:1598,1747,3574,820`                                                                                 |
| jj / hg / gitbutler               | zero code; roadmap group AS only                                                                                                                                                                                                                                                                                                                                                 | `tasks.md:1311`                                                                                                                      |
| doctor                            | probes engine selection + `git`/`gh` presence + merge-guard install; nothing about git version floors, signing, or drivers                                                                                                                                                                                                                                                       | `crates/thegn-host/src/cmd/doctor.rs:772,1081`                                                                                       |

The change is shaped by that audit: extend the existing chokepoints
(`fold.rs`, `plumbing.rs`, `integrate.rs`, `GitConfig`), never a parallel
mechanism.

## 2. Land strategy in the object DB

`config_enum! LandStrategy: "land strategy" { Merge = "merge", Squash =
"squash", Rebase = "rebase" | "linear" } default = Merge`, key
`[merge_queue] land_strategy`, overridable per workspace via the existing
`MergeQueueOverlay` (exhaustively destructured, so the compiler enforces the
overlay grows with the struct).

All three strategies keep the invariant that the target ref only advances by
object-DB fold + CAS, gated first:

- **merge** — today's path, unchanged.
- **squash** — same `merge_tree` result, but `commit_tree` with a **single**
  parent (the running tip); message template gains the folded subjects.
- **rebase** — replay the branch's commits one at a time:
  `merge-tree --write-tree --merge-base <parent> <tip> <commit>` per commit
  (a plumbing cherry-pick), each committed with a single parent. Any
  conflicting step defers the branch exactly like a conflicting merge fold —
  no partial replays land.

A branch already an ancestor of the tip stays a no-op under every strategy.
`merge_msg` becomes a template (`[merge_queue] land_message`) rendered with
the same brace-var engine `agent_task` prompts use (`{branch}`, `{target}`,
`{subjects}`) — one template engine, not two.

Rebase changes committer/author mapping: replayed commits keep the original
author, committer is the ambient git identity — same as `git rebase`. Noted
in the config docs, not softened.

## 3. Signing thegn's own commits

Two distinct fixes:

- **`snapshot_worktree` is a bug today**: it is a background commit that can
  hit a pinentry prompt. It gains `gpg_args(override_gpg)` like every other
  background history operation. No new config.
- **Fold/land signing is a policy**: `[merge_queue] sign_commits = false`.
  When on, `commit_tree` passes `-S` (which honors the user's
  `gpg.format`/`user.signingkey`, so GPG vs SSH signing needs no thegn
  switch). The guard rails:
  - The signing invocation runs with the same null-stdin, `GIT_TERMINAL_PROMPT=0`
    discipline as `run_w`; a gpg/ssh-agent that would prompt fails instead.
  - A signing failure classifies like `GateClass::Error` — an infrastructure
    fault that stops the drain with a clear reason. It never marks the branch
    `needs_human` and never wakes the fixing agent, because the branch is not
    at fault (the same never-blame-a-good-branch rule the gate wrap follows).
  - `thegn doctor` reports signing readiness by performing a cheap
    non-interactive probe (sign a test blob under the active identity) only
    when `sign_commits` is on — the general Probe contract stays "no network,
    cheap", and this one is opt-in posture, not a seam probe.

Identity is out of scope: which `GNUPGHOME`/key signs is the active
environment's business (`add-decoupled-identities` binds it per profile/
bundle; the separately-scoped credential-broker work manages material). This
change never reads or stores key material.

## 4. Merge drivers and rerere in the fold

**Open question resolved by an audit task, not an assumption:** whether
`merge-tree --write-tree` invokes custom `merge=<driver>` drivers via
ll-merge is version-dependent and undocumented; task 5.1 pins the answer per
supported git version with a fixture repo. The spec'd behaviour is
implementation-order independent:

- At fold time, conflicted paths are checked against the repo's
  `.gitattributes` `merge=` declarations (`git check-attr merge --
<paths>` — batched, off-loop). If a conflicted path is governed by a custom
  driver that the object-DB fold did not honor, the branch is folded through
  the **throwaway-worktree real-merge path** — the same machinery
  `regenerate_merge` already uses for lockfiles — where `git merge` runs the
  driver, and the resulting tree feeds back into the fold. Clean folds never
  pay this cost.
- `[merge_queue] rerere = false` (opt-in): when on, the reused gate worktree
  (`gate_reuse_worktree`) and the driver-merge worktree run with
  `-c rerere.enabled=true` sharing `<git-common>/rr-cache`, so a conflict
  resolved once (by the user or the handoff agent committing the resolution)
  auto-resolves on the next drain. This targets the observed dominant
  conflict class — the same fix hunk conflicting drain after drain. rerere
  never _lands_ anything by itself: an auto-resolved merge still runs the
  gate.
- **X 314 (weave merge driver) is explicitly not this.** That item is thegn
  _authoring_ a semantic driver; this change only stops thegn _bypassing_
  drivers the repo already declares.

## 5. jj and Mercurial: the honest seam answer

**Question:** is jj support a provider seam or out of scope?

**Answer: a seam eventually (roadmap AS 587), coexistence now — and the spec
records why the seam is not this change.**

- The tempting framing — "swap `GitBackend` impls" — is wrong. `GitBackend`
  is a _read-engine_ seam (gix vs CLI over identical git semantics). thegn's
  substance is the write layer: `worktree add/remove`, the fold's
  `merge-tree`/`commit-tree`/`update-ref` CAS, the merge guard hook, branch
  checkout resync. Those are git-porcelain-shaped, and `GitLoc` itself is
  `git -C <path>`-shaped. A `VcsBackend` seam is real work at the layer AS
  587 describes, with `caps ⇔ optional ops` doing heavy lifting (jj has no
  index: `stage`/`unstage` are capability-absent, not emulated).
- The deeper mismatch is the **workspace model**: jj does not support
  `git worktree` at all — its analogue is `jj workspace`. thegn's core
  contract "each git worktree is a tab" therefore has no mechanical mapping;
  jj parity means teaching the worktree lifecycle a second backend
  (AS 589/590 territory), not just the diff/status reads.
- What users hit _today_ is the coexistence case: a colocated repo
  (`.jj/` beside `.git/`) where jj docs say external tools should "mostly use
  read-only git commands". thegn's read path (gix + CLI reads) is already
  safe there. The hazards are: detached HEAD is jj's _normal_ state (must not
  render as an error), jj **ignores the git index** (thegn's staging UI is
  misleading and its `git apply --cached` writes can be clobbered by jj's
  next auto-snapshot), background `git fetch` interleaving is called out by
  jj's own docs, and jj's conflict markers appear as `.jjconflict-*` trees.
  So this change ships **detection + degradation**: doctor line, sidebar
  badge, staging-surface notice, colocated repos excluded from `auto_fetch`
  unless `[git] auto_fetch_colocated = true`. Mutations are warned, not
  blocked — thegn is not jj's police, and blocking would strand users who
  know what they're doing.
- **Mercurial is ruled out**, on the record: ~3% and falling, no hosting
  momentum, zero mentions in this codebase or its roadmap, and none of the
  worktree/fold substance transfers. If AS ever lands the `VcsBackend` seam,
  hg is at most a `reserved` kind, never a commitment.

Detection is pure and cheap (a `.jj` directory check per workspace root,
cached with the glyph scan) — no `jj` subprocess is ever spawned in this
change.

## 6. Structural diff view (difftastic)

- **Read-only surfaces only.** The `SANITIZED_DIFF` contract is untouched and
  restated in the spec: anything that feeds `git apply`/staging pins
  `--no-ext-diff`. Structural rendering applies to the `Alt /` DiffView
  modal and `thegn diff --structural`; the panel's inline hunk previews keep
  the internal parser (they are hunk-addressable and feed staging entry
  points).
- **Invocation:** `difft --color always --width <content width>
--background <from theme mode> <old> <new>` via `GIT_EXTERNAL_DIFF`
  semantics on a `git diff` run _without_ the sanitizers (or direct file
  pairs from `cat-file` temp files — decided at implementation by whichever
  keeps rename handling sane). Every flag has a `DFT_*` env twin, which is
  the mechanism when routing through git. Guards: `--byte-limit`/
  `--graph-limit` set from config-defaulted caps, a wall-clock timeout, and
  any non-zero/oversize/parse-error outcome falls back to the internal
  unified view with a one-line notice — difftastic's own README is candid
  that it scales poorly on large change sets.
- **Rendering:** difft emits SGR ANSI. A new **pure** `thegn-core` parser
  (ANSI SGR subset → styled cell runs) converts it for composition; truecolor
  is composed and quantized once at the existing `wire.rs` chokepoint, so no
  color literals appear at draw sites and the degradation invariant holds.
  The parser is substrate-free and lands under the 95% line gate with
  fixture-driven tests (recorded difft output).
- **Selection:** `[git] structural_diff = "off" | "auto" | "difft"`
  (default `off`; `auto` = structural when the tool resolves, internal
  otherwise), plus a toggle key inside the DiffView modal (new action id ⇒
  help page + ratchet). `difft` becomes a managed tool
  (`Source::GithubRelease`, `path_fallbacks: ["difft"]`, pinned version) so
  the three-tier override/PATH/managed resolution and doctor Probe come for
  free — and the spec'd source kinds (GitHub release) are respected, avoiding
  the known managed-tools spec drift around `Source::Cargo`.
- **Known trap, decided here:** the default `[[tools]]` seed includes
  `diff = "git diff"`, which makes the native DiffView modal unreachable on a
  default install (`Action::Diff` prefers the tool). The structural view
  toggle lives _inside_ the modal, so this change keeps the seed as-is and
  routes `structural_diff != off` ahead of the `[[tools]] diff` lookup —
  setting the key is an explicit opt-in that should win over a seeded
  default. Documented in the config example.

## 7. Event loop, rendering, schema, help (config.yaml checklist)

- **Wake path:** difft runs as a subprocess off-loop (`spawn_blocking` /
  `sched::spawn_bg`), result over a channel + one `TerminalWaker` pulse;
  `check-attr` and the signing probe likewise. jj detection piggybacks the
  existing glyph-scan cadence. Nothing polls; nothing new runs before the
  first frame.
- **Damage channels:** the DiffView modal and sidebar badge are chrome ⇒
  `Full` on change, exactly like today's modal open/close. No pane-output
  interaction; `render_plan` tests are untouched.
- **SQLite:** no schema change. (The jj flag rides the in-memory glyph model;
  if persistence proves wanted it joins the existing `glyph_cache` JSON
  without a `user_version` bump.)
- **Help:** the structural-toggle action id and any changed DiffView key
  hints claim a `docs/help/` page (help + prose ratchets); the config
  reference page is generated, never hand-written. New badge glyph goes
  through `caps::active_glyphs()` (glyph-literal ratchet).

## 8. Security

- **Credential handling:** no new secrets, no key material. Signing delegates
  to the ambient gpg/ssh agent under the active identity
  (`add-decoupled-identities`); thegn passes `-S` and never sees a key. The
  doctor signing probe signs a throwaway blob and discards it. No raw tokens
  in config; no new `SecretRef` uses.
- **Subprocess surface:** `difft` executes with repo file content as input —
  untrusted input to a native parser. Mitigations: pinned managed-tool
  version, no network access needed or granted, bounded by timeout +
  byte/graph limits, runs under the same background-job ceilings
  (`wrap_background_argv` slice) as the gate when invoked from queue paths.
  Its output is parsed by thegn's own SGR-subset parser — unknown escape
  sequences are stripped, never forwarded to the terminal raw, so a
  malicious file cannot smuggle terminal control sequences through the diff
  view.
- **Custom merge drivers are user-configured arbitrary commands** run by git,
  not thegn — but the fold newly _routes into_ them. They only run for repos
  the user opened, from git config the user (or repo, via `.gitattributes` +
  driver definitions that must live in trusted git config, never in-repo)
  controls; the worktree-merge path runs under the merge-queue's existing
  resource ceilings. No thegn config key can define a driver command.
- **Blast radius of new writes:** signing changes commit objects only;
  strategy changes what the fold commits — both still behind the gate and the
  CAS advance, and both default to today's behaviour. rerere shares
  resolutions within one repo's `rr-cache` only.
- **jj detection is read-only** (a directory existence check).
- All new keys resolve from the **trusted** config layers only; the untrusted
  `.thegn.*` repo overlay cannot set them (reconciled with
  `add-config-trust-resolution`).

## 9. Open questions

1. Does `merge-tree --write-tree` honor custom ll-merge drivers on the git
   versions thegn supports? (Task 5.1 answers with fixtures; the routing
   design works either way — it only changes how often the worktree path is
   taken.)
2. Should `land_strategy = "rebase"` update the _branch ref_ to the replayed
   commits after landing (so the worktree shows the landed linearized
   history), or leave the branch untouched like today's merge fold? Leaning
   untouched (least surprise; the worktree is removed on land anyway when
   lifecycle policy says so).
3. Signature _verification_ surfacing (`%G?` badges in the commits view) —
   deferred; read-side verification is a different feature with its own
   trust-model questions (whose keyring?).
4. Should `auto` structural diff also take over the panel's Full-width gitfull
   diff region? Deferred until the modal path proves the ANSI-cells renderer.
