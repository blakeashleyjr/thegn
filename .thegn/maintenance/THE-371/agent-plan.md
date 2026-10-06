# THE-371 plan

Scope: replace util.rs strip_stray_core_worktree (read_to_string + fs::write, follows links, lines() rewrite)
with crates/thegn-core/src/git_config_heal.rs: .git must be a real owned dir, config opened O_NOFOLLOW,
git-style config.lock (O_EXCL) -> write -> fsync -> revalidate -> rename; byte-exact scanner (CRLF, BOM,
continuations, non-UTF-8) that refuses unparseable syntax. Refusal skips the checkout resync.
fsperm gains create_owner_only_exclusive, owned_by_current_user, rename_replace.
Tests: byte-scanner unit tests + tempdir symlink/dir/held-lock/refuse tests. Out of scope: other control-file repairs (none found).
