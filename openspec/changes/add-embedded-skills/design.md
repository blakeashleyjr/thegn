# Design — embedded skills

## Model

A skill is a directory with a `SKILL.md` (frontmatter `name` + `description`,
prose body; the format Claude Code, superset, and memex already consume),
embedded via `include_str!`/`include_dir` so the set is versioned with the
binary — no network, no registry, no DB. The curated v1 set teaches thegn's
own workflows (worktrees, merge queue, PR queue, self-docs via
`thegn mcp serve`); `extensions/skills/mq` graduates into it, `tui-check`
stays repo-internal (it drives muse against a dev build — meaningless on a
user machine).

## Distribution: sync, not injection

`skills sync` resolves target directories per agent kind through small
adapter files (vendor paths only in impls, the seam rule): Claude Code →
`~/.claude/skills/thegn/<name>/`, generic → `~/.agents/skills/thegn-<name>/`
(the cross-agent convention superset and memex both ship to), others
`reserved`. The `[[agents]]` list picks default targets, same as
`mcp wire`.

Sync semantics (the superset model, spec'd hard):

- every written file carries a managed marker (frontmatter key) + content
  hash + the shipping thegn version;
- idempotent: same version ⇒ no writes; upgrade ⇒ replace marked files;
- deprecation: a marked skill no longer in the embedded set is deleted;
- user files — unmarked, or marked-but-hand-edited (hash mismatch) — are
  never overwritten or deleted; a hash mismatch is reported and skipped.

The hash-mismatch-skip rule is the interesting call: superset overwrites its
managed files unconditionally; thegn treats a user edit as adoption. Rationale:
silently reverting a user's fix to a recipe is the same sin as clobbering
their config, and doctor makes the divergence visible instead.

`auto_sync` (off by default) runs the same sync on host startup via
`spawn_blocking` — off the event loop, after the first frame, failure is
best-effort (status line, never fatal). No new tick, no render-plan impact,
no daemon involvement.

## The drift gate

Embedded recipes rot when the CLI moves. A unit test walks every embedded
`SKILL.md`, validates frontmatter, extracts fenced `thegn …` command lines,
and checks each against the live clap definition (subcommand path resolves;
placeholder args `<...>` skipped). This is the help-ratchet idea applied to
shipped prose: the gate is a test in `just test`, not convention. Pure
manifest/marker/merge logic lives in `thegn-core` (95% gate); filesystem
walking and vendor paths in host.

## Reconciliation with `add-skills-registry`

That change specifies a versioned third-party package registry with
request-time, cache-aware injection through `thegn-proxy`/ACP — all excised
machinery (its delta adds an `ai-gateway` capability that cannot exist
today). It is not edited here. The layering if the AI track reopens: embedded
skills are one (trusted, version-locked) package source; a registry adds
acquisition of third-party packages; injection adds request-time selection.
Nothing in this change forecloses that, and nothing depends on it.

## Alternatives considered

- **Provision at every app launch (superset):** thegn is a long-lived TUI,
  not a desktop supervisor of agents; unconditional startup writes to home
  directories violate least surprise. Explicit `sync` + opt-in `auto_sync`.
- **Skills as a `thegn mcp serve` resource:** agents can already read docs
  there, but skills are load-bearing precisely because harnesses discover
  them in their own skill directories without being told; an MCP resource is
  invisible to the harness's skill loader. Both channels coexist.
- **A `[skills.custom]` user-package mechanism:** deferred — it reopens the
  supply-chain question (unreviewed prose steering agents) that
  embedded-only closes.

## Security

- **Write surface:** agent skill directories in `$HOME`. Bounded by: fixed
  thegn namespace subpaths, marker+hash discipline (never touch unmarked or
  user-modified files), explicit invocation (or explicit `auto_sync` opt-in),
  `--remove` reversibility, and skill `name`s validated as path-safe single
  segments (no separators, no `..`) before any path is built.
- **Content trust:** embedded skills are repo-reviewed, ship in the signed
  release binary, and contain prose only — thegn never executes them. No
  third-party acquisition means no new supply chain. The drift gate keeps
  cited commands real, which also limits what a stale recipe can mislead an
  agent into running.
- **Prompt-injection posture:** a skill instructs agents to run `thegn`
  verbs; recipes must not embed instructions that bypass agent approval
  flows (e.g. no `--force`/`--yes` in recipe command lines unless the recipe
  explicitly tells the agent to confirm with the user first — a review rule
  for the curated set, noted in the manifest test as a lint on those flags).
- **No secrets involved:** skills carry no credentials, tokens, or env.

## Open questions

- Which additional vendor skill dirs are worth first-party adapters (codex?
  gemini?) versus riding the `~/.agents/skills/` convention — resolve at
  implementation time from what those CLIs actually load.
- Should `wt`-lifecycle and merge-queue recipes merge into one "thegn
  workflows" skill or stay one-per-workflow (better selective `exclude`)?
  Default: one-per-workflow.
