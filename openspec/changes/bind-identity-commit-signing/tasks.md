# Tasks — bind identity commit signing

Owner: THE-149 (Runtime Security & Host Architecture). THE-122 owns credential
custody and is not the owner of this split execution feature.

## 1. Preserve the shipped foundation

- [x] 1.1 `[identities.<name>.signing]` parses `format = "openpgp" | "ssh"`
      plus `key`, and the pure resolver returns format/key arguments with SSH
      path expansion and redaction-safe tests.
- [x] 1.2 The config example documents the schema and states that
      per-operation controls layer above the identity default.

## 2. Decide and encode signing semantics

- [ ] 2.1 Decide whether a non-empty identity block implies
      `commit.gpgSign=true` or add an explicit enable field; update schema,
      defaults, JSON schema, config example, and migration notes so the accepted
      no-per-commit-flags scenario is true.
- [ ] 2.2 Represent signing as typed composition data, not private key bytes or
      a shell fragment, and define remote-provider behavior for host-only paths.

## 3. Compose every pane path

- [ ] 3.1 Fold the resolved signing binding through profile, bundle, global,
      workspace, and worktree identity scopes with worktree precedence.
- [ ] 3.2 Apply it to both host and sandbox pane launches without mutating
      user-owned gitconfig; test that an unbound sibling has no injected keys.
- [ ] 3.3 Either safely provision/mount remote signing material or reject an
      unusable remote binding before launch with an actionable error.

## 4. Preserve operation-level precedence

- [ ] 4.1 Prove the commit overlay's inherit/sign/no-sign state overrides the
      identity default for its operation.
- [ ] 4.2 Prove `[git] override_gpg` disables identity-default signing for each
      supported background rewrite path.
- [ ] 4.3 Document how `[merge_queue] sign_commits` selects the resolved
      identity in the gate worktree and how a signing failure remains an
      infrastructure error.

## 5. Executable evidence and docs

- [ ] 5.1 With a throwaway SSH signing key, prove a bound worktree creates and
      verifies a signed commit while an unbound sibling follows repo/global
      config; prove the explicit no-sign override creates an unsigned commit.
- [ ] 5.2 Add dedicated `docs/help/` prose for identity signing, worktree scope,
      host/sandbox/remote behavior, and operation-level precedence.
- [ ] 5.3 Run focused core/host tests, strict OpenSpec validation, and the full
      repository gate before archive.
