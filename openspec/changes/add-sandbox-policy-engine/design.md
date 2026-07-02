# Design

## The decision (pure, core)

`policy::decide(op, cfg, ctx) -> PolicyDecision` where:

- `Operation { kind: Read | Write | Delete | Shell, path: Option<PathBuf>,
command: Option<String> }`
- `PolicyDecision { Allow, Deny { reason }, Ask { prompt } }`
- `PolicyCtx { worktree_root: PathBuf }` — so rules can express "under the
  worktree" relative to the active root.

Evaluation: walk `cfg.rules` in order; a rule matches when every present selector
matches (`ops` subset, `path` glob — evaluated relative to `worktree_root` for
`under`/`outside` helpers, `command` regex). First match wins; its action is the
decision. A rule marked `stop` short-circuits. No match → the configured
`default` (recommended `ask` for writes/shell, `allow` for reads under the
worktree). All pure and **unit-tested** to the 95% core gate.

Built-in hard denies (always evaluated first, not overridable by user allow):
writes/deletes to `.git/config`, `~/.ssh`, `~/.gnupg`, and the configured secret
paths — mirrors the "shared .git/config is read-only in the sandbox" invariant
already in the sandbox spec.

## Config (superzej-core, config.rs)

```toml
[policy]
default_read  = "allow"   # under the worktree
default_write = "ask"
default_shell = "ask"

[[policy.rules]]
ops  = ["read"]
path = "under:."          # worktree-relative
action = "allow"

[[policy.rules]]
ops  = ["write", "delete"]
path = "outside:."
action = "deny"

[[policy.rules]]
ops  = ["shell"]
command = "^(rm -rf /|curl .*\\| ?sh)"
action = "deny"
```

`PolicyConfig` uses `serde(default)` (existing configs keep working; empty policy
= today's behavior via the defaults). `PolicyAction` (`Allow|Deny|Ask`) uses
`config_enum!`. Layerable per the standard precedence (global → profile →
workspace → repo) following `effective_keybinds` / `repo_sandbox`.

## Enforcement seam (host / bouncer)

The bouncer's tool broker (`[llm_proxy].bouncer`) already services `fs/read`,
`superzej/edit`+`write`, and `terminal/create` over unix-socket ACP with an
approval step. Insert `policy::decide` **before** servicing each brokered op:

- `Allow` → service it (no prompt).
- `Deny` → refuse with the reason (maps to ACP `reject_once`), surfaced inline.
- `Ask` → raise the **existing** ACP permission overlay (R 232). An
  `allow_always` / `reject_always` choice appends a corresponding rule to a policy
  overlay so the next identical op is decided without a prompt.

## Invariants

- **Event loop**: `decide` is pure and cheap; it runs on the broker's off-loop
  task, not the render loop. `Ask` raises the overlay via the existing
  permission-request path (channel + `TerminalWaker`). No new timer.
- **Render**: the permission overlay is existing chrome (a `Full`/overlay frame);
  `Allow`/`Deny` decisions render nothing new. render_plan invariants unchanged.
- **State**: no `user_version` bump. Persisted `always` choices append to config /
  a policy overlay file, not a DB table.
- **Additivity + safety floor**: the container/bouncer isolation is unchanged and
  remains the hard boundary; policy only _narrows_ what a brokered op may do. An
  empty `[policy]` reproduces today's behavior.

## Alternatives considered

- **Bake policy into seccomp/bwrap profiles** — too coarse and per-backend; the
  broker seam is where superzej already mediates file/shell ops uniformly.
- **Prompt on everything (status quo)** — high friction; the whole point is to
  auto-allow the safe common case and reserve prompts for genuinely risky ops.
