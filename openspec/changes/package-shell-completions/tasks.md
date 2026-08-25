# Tasks

## 1. Packaging (nix)

- [ ] 1.1 `nix/package.nix`: add `installShellFiles` to `nativeBuildInputs`
      and the guarded `postInstall` completion loop (both `binName` and
      `aliasName`, bash/zsh/fish, `canExecute` guard, throwaway
      `HOME`/`XDG_*`).
- [ ] 1.2 Verify the dev channel: `nix build .#dev` (or the dev attr) yields
      `_thegn-dev` / `_tg-dev` etc. with no collision against the default
      package's files.
- [ ] 1.3 Sanity-run: `nix build` then check
      `result/share/zsh/site-functions/_tg` starts with `#compdef tg` (the
      alias-name proof).

## 2. Docs

- [ ] 2.1 `docs/help/cli.md`: extend the completions line with the per-shell
      static/eval one-liners for non-nix installs.
- [ ] 2.2 README install section: one sentence — nix installs completions
      automatically; tarball users run `thegn completions <shell>` (link the
      help page).

## 3. Validation

- [ ] 3.1 Run `just ci` once, when the implementation is complete (its
      `nix-build` step gates the packaging; includes
      `openspec validate --all --strict`).
