# Seed embedded agent skills into worktrees

Linear: THE-20

## Why

Thegn needs versioned operational guidance that can be consumed by the agent
harnesses it launches without a network registry or writes into a harness's
home directory. The shipped design is a local registry plus conservative,
project-local seeding. It deliberately does not implement the earlier
home-directory `sync` proposal.

## Delivered design

- The binary embeds the `mq`, `pipeline`, and `supervise` skill packages and
  can merge additional packages from `[skills].user_dirs`.
- `thegn skills list`, `show`, and `seed` inspect or materialize the registry.
- Eligible skills seed into a worktree's harness-native directories
  (`.claude/skills`, `.agents/skills`, `.pi/skills`) during create/startup or
  an explicit seed. Thegn never writes harness home directories.
- Strict, bounded frontmatter names the skill, description, harnesses,
  capability gate, and eligible seed phases. User discovery accepts only
  immediate, non-symlink `<name>/SKILL.md` packages.
- Managed markers and content hashes make upgrades conservative: unmarked or
  user-modified files are preserved, and retired managed files are removed
  only when the registry survey is complete and the file is unchanged.
- `[skills] enabled`, `user_dirs`, and `exclude` control the feature. Automatic
  seeding defaults on; explicit seeding remains available when it is off.

## Non-goals

- A remote skill registry, installer, or executable plugin runtime.
- Synchronizing `~/.claude`, `~/.agents`, or other harness home directories.
- Executing skill prose or treating it as trusted code.
- Overwriting unmarked or locally modified files.

## Evidence

Implemented in `thegn-core::skills`, `config_skills`, the host `skill_seed`
adapter and `thegn skills` command, with documentation and command-drift/unit
coverage. Principal delivery commits: `8a405f1d`, `7d6cf8c7`, `af2ff412`,
`9d5648ca`, and `ff15d8a9`.
