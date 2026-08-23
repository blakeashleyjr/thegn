## 1. Seam conversions (all: `async fn` → `BoxFuture`, bodies wrapped, behavior identical)

- [x] 1.1 Issue: `IssueBackend` object-safe; `RouterInner` deleted; router stores `Box<dyn IssueBackend>`
- [x] 1.2 Calendar: `CalendarBackend` object-safe; delegation enum deleted; `Box<dyn CalendarBackend>`
- [x] 1.3 Media: backend trait object-safe; `MediaClient` match dispatch replaced with trait objects
- [x] 1.4 Provider: `RemoteProvider`/`ProviderEgress`/`ProviderCheckpoints`/`ProviderFiles` object-safe; `enum Provider` routes via `Option<&dyn Trait>` accessors (caps/Iroh facade unchanged)

## 2. Gate

- [x] 2.1 `test/async-trait-ratchet.txt` reseeded to zero entries (header kept)
- [x] 2.2 clippy (core/svc/host/media) + per-seam test suites + `just lint`; openspec validate
