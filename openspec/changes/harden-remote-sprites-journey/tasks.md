# Tasks

## 1. Ledger and reap

- [x] 1.1 Record before create, proof before delete, unrecorded sprites never deleted
- [x] 1.2 `env reap --provider sprites` and `env down --all` skip counts

## 2. Hold, size, background creates

- [x] 2.1 Headless keep-awake hold with ceiling
- [x] 2.2 `min_ram_gb` memory policy; size warnings
- [x] 2.3 `eager` never creates a sprite in the background

## 3. Cost, results, conformance

- [x] 3.1 Sprites block in `fleet cost`
- [x] 3.2 `agent run --wait --pull`
- [x] 3.3 `provider_conformance.rs` first slice

## 4. Live verification (THE-2234)

- [ ] 4.1 Verify the memory-policy endpoint accepts `limit_mb` live
- [ ] 4.2 Verify the Tasks API hold keeps a sprite awake to the ceiling live
- [ ] 4.3 Verify GET of a deleted sprite returns 404 live
- [ ] 4.4 Verify the usage API field units live
- [ ] 4.5 Verify `created_at` is present on every sprite live
- [ ] 4.6 Verify a Vercel stopped sandbox resumes on pane open live
- [ ] 4.7 Live-check fleet `down`/`unrecorded`, `env reap --unrecorded`, paged listing, git audit file and Vercel stderr with `test/sprites-live.sh`
