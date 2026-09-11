# Bind commit signing to the resolved identity

## Why

The identity config schema can describe a signing format and key, and core can
resolve that pair into Git `-c` arguments. No pane, sandbox, or host Git-process
composition path consumes those arguments today. Treating the helper as a
delivered feature would therefore claim signing behavior that users do not get.

The original scenario also requires a policy decision: a non-empty identity
signing block must either enable signing automatically or select a key only when
repo/global or per-operation policy already enables it. That decision belongs
with execution and precedence, not credential storage.

## What Changes

- Define whether a non-empty identity signing block enables commit signing, and
  encode the default explicitly.
- Inject the resolved `gpg.format` and `user.signingKey` into both host and
  sandbox pane Git environments without mutating user-owned gitconfig files.
- Preserve global → workspace → worktree identity precedence; an unbound
  sibling continues to use repo/global Git configuration.
- Keep the commit overlay signing cycle and `[git] override_gpg` above the
  identity default, with executable precedence tests.
- Document interactive panes, background rewrites, and merge-queue folds in
  `docs/help/` and the config example.

## Scope and ownership

The schema and pure resolver are already implemented groundwork. Runtime
composition, semantic policy, integration tests, and docs remain unimplemented.
This focused change was split from `add-credential-broker` so THE-122 can close
on credential custody without losing the signing requirement. THE-149 owns the
remaining Runtime Security & Host Architecture work; it is deliberately not
reassigned to THE-122 or to the unrelated SCM-greenfield issues.

## Impact

- **Code:** `thegn-core::bundle` identity composition plus the host/sandbox pane
  launch boundary; Git operation precedence tests.
- **Config:** no schema break; consumes the existing
  `[identities.<name>.signing] { format, key }` fields.
- **UI/API:** no new action, keybinding, capability, or control endpoint.
- **Security:** signing keys stay path/key identifiers; the change must not copy
  private key material into env, argv, logs, or state DB.
