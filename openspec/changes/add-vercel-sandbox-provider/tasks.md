# Tasks

## 1. Spine

- [x] 1.1 Register `EnvProviderKind::Vercel`, `Provider::Vercel`, factory, stubs (THE-2223)

## 2. Control plane and bridge

- [x] 2.1 Control-plane client with redacted token
- [x] 2.2 `thegn vercel-exec` bridge: capture, TTY, stream fallback, keep-alive

## 3. Credential-free transfer

- [x] 3.1 Credential-free provisioning: bundle seed, stripped steps, applier refusal

## 4. Ledger, caps and reaper

- [x] 4.1 Ledger, account fingerprint, `max_instances`
- [x] 4.2 Reaper: stop at ceiling, delete after grace, doctor listing

## 5. Surface and docs

- [x] 5.1 `thegn env pull` support and admission refusals (host_cache, ssh connect)
- [x] 5.2 Help page, provider-impl checklist, config example

## 6. Live verification

- [ ] 6.1 Run `test/vercel-live.sh` against a real Vercel account (THE-2234) and settle the unverified API facts
