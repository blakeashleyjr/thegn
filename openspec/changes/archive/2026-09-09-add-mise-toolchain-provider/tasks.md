# Tasks — mise activation provider

## 1. Detection and core policy

- [x] 1.1 Keep local and remote detection aligned across mise config variants,
      bounded `conf.d`, `MISE_ENV`, `.tool-versions`, and language pin files.
- [x] 1.2 Add the generic activation policy/layer/provider-state seam and
      `auto|shims|env|off` configuration with unit coverage.
- [x] 1.3 Derive content identities from canonical detected files plus
      `mise.lock`, with edit invalidation tests.
- [x] 1.4 Compose bundle > devshell > mise > base using fill-only env merge and
      credential filtering, with precedence tests.

## 2. Host and target adapter

- [x] 2.1 Implement `mise_provider` local/remote activation, owner-only caches,
      cache-only launch behavior, and asynchronous refresh/waker integration.
- [x] 2.2 Gate environment resolution and explicit local/target installs on the
      current Thegn approval; never call `mise trust`.
- [x] 2.3 Keep normal launch install-free and make explicit install call only
      bounded `mise install` after identity revalidation.
- [x] 2.4 Report cached/presence-only provider, tier, inject, state, trust,
      files, shims, and reason diagnostics.

## 3. Documentation and reconciliation

- [x] 3.1 Document injection modes, precedence, trust, explicit install, and
      degradation in example config/help surfaces.
- [x] 3.2 Reconcile the OpenSpec with the shipped provider adapter and validate
      it with `openspec validate add-mise-toolchain-provider --strict`.
