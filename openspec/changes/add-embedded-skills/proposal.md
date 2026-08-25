# Add embedded skills — thegn-shipped agent recipes, installed into agent CLIs

Linear: THE-20

## Why

Agents drive thegn best when they are taught to: the repo already carries
`extensions/skills/{mq,tui-check}/SKILL.md`, and the `mq` skill is exactly the
kind of recipe every _user's_ agent should have — but today nothing ships
those skills to users, so each agent rediscovers `thegn wt`/`merge`/`pr
queue` from `--help` or not at all. The reference model is superset.sh: a
curated skill set embedded in the product, provisioned into standard agent
skill directories (`~/.claude/skills/…`, the emerging `~/.agents/skills/`
convention), kept updated, and marker-tagged so user-authored files are never
touched. memex uses the same distribution channel, which is evidence the
directory convention is becoming the cross-agent standard.

The in-flight `add-skills-registry` change covers adjacent ground but is
built entirely on the **excised** AI gateway: its distribution is request-time
injection through `thegn-proxy`/ACP (its delta even creates an `ai-gateway`
capability), which no longer exists — the same trap the brief flags for
`add-fleet-view`. THE-20's substance — skills _shipped with thegn_ and placed
where agents already look — needs none of that: it is file distribution, the
same shape as `thegn mcp emit`/`wire`, fully AI-free-shell compatible. This
change scopes that delta and nothing of the registry's third-party-package or
injection ambitions.

## What Changes

- **New capability `skills`.** A curated set of `SKILL.md` packages embedded
  in the `thegn` binary at build time (versioned with the binary), teaching
  agents to drive thegn: worktree lifecycle (`wt`), merge queue (grown from
  `extensions/skills/mq`), PR queue, and pointing agents at `thegn mcp serve`
  for self-docs. (`tui-check` stays a repo-development skill; not shipped.)
- **CLI:** `thegn skills list`, `thegn skills show <name>`, and
  `thegn skills sync [--agent <kind>] [--remove]` — sync installs/updates the
  embedded set into supported agent CLIs' skill directories via per-vendor
  adapters (claude + the generic `~/.agents/skills/` convention implemented;
  others reserved), removes deprecated thegn-managed skills, and never
  touches files it did not mark.
- **Managed markers:** every installed file carries a thegn-managed marker +
  content hash + shipping version, so sync is idempotent, upgrades replace
  only thegn's files, and user-authored skills at the same paths are
  untouchable (the superset model).
- **Config:** `[skills]` — `enabled`, `auto_sync` (default off; when on, sync
  runs off the event loop after startup, never before the first frame),
  `exclude` (skill names to withhold).
- **Drift gate:** a unit test validates every embedded skill's frontmatter
  and checks the `thegn` command lines its body cites against the live clap
  tree, so shipped recipes cannot rot as the CLI evolves.
- **Doctor:** reports per agent kind: skills dir found, installed
  thegn-managed skills current/stale/absent.

## Impact

- **Specs:** new `skills` capability (ADDED). No DB change, no new state —
  the embedded set + the marker files on disk are the whole truth.
- **Roadmap:** the AI-free delivery of the skill half of **AR 544/548**
  (skill registration/prompt library — re-grounded off the excised proxy);
  complements **AL 455** (`thegn mcp serve` gives agents thegn's docs; skills
  give them the workflows).
- **In-flight reconciliation:**
  - `add-skills-registry` — referenced, not built on: it depends on the
    excised LLM proxy (`ai-gateway` delta). This change is the AI-free
    distribution substrate; if the AI track reopens, a registry/injection
    layer can treat embedded skills as one package source. THE-20 is covered
    **here**, not there.
  - `add-mcp-proxy-hub` (sibling change, THE-16/49) — no hard dependency;
    skill-shaped memory tooling (memex-style) is distributed through this
    channel while MCP-shaped memory rides the proxy's presets.
  - `add-cli-namespaces-and-remote-open` — `thegn skills` follows its
    noun-verb grammar; the drift-gate test validates against whatever clap
    tree that change lands.
- **Config:** `[skills]` documented in `config/config.toml.example`;
  `docs/help/` updated per the config-table rule.
- **Non-goals:** no third-party or user skill registry/marketplace (embedded,
  reviewed-in-repo content only — that keeps the supply chain closed); no
  request-time selection/injection (excised-proxy territory); no skill
  _execution_ (skills are prose recipes; the agent runs the commands); no
  automatic writes to agent directories without `sync` being invoked or
  `auto_sync` being explicitly enabled; no TUI surface.
