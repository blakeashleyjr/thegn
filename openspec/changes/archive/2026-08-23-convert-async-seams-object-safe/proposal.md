## Why

Four seams still declared their traits with `#[allow(async_fn_in_trait)]` — issue trackers, calendar, media, and the managed-sandbox provider quartet (`RemoteProvider`/`ProviderEgress`/`ProviderCheckpoints`/`ProviderFiles`). `async fn` in a trait is not object-safe, so each seam carried a hand-written delegation enum (`RouterInner`, calendar `Inner`, `MediaClient`, and ~24 match blocks on `enum Provider`) that must be edited in N places for every new provider — exactly the rewrite-per-provider the provider-seams spec exists to prevent. `test/async-trait-ratchet.txt` pinned 8 files of this debt.

## What Changes

- Every remaining async seam trait converts to the house `BoxFuture` shape (`ControlApi` precedent): `fn m<'a>(&'a self, …) -> BoxFuture<'a, R>`; impl bodies wrapped in `Box::pin(async move { … })`, behavior identical.
- Issue: `RouterInner` deleted; the router stores `Box<dyn IssueBackend>`.
- Calendar: the delegation enum deleted; `Box<dyn CalendarBackend>`.
- Media: the `MediaClient` match dispatch replaced with trait objects.
- Provider: the four traits object-safe; `enum Provider` STAYS as the capability facade (Iroh exec-only special cases, `caps()` table, `name()`) but routes through `Option<&dyn Trait>` accessors instead of N-arm matches.
- `test/async-trait-ratchet.txt` burns to zero entries (shrink-only allowlist now empty).

## Impact

- tasks.md row A6.
- Specs: provider-seams (delta: no remaining `async_fn_in_trait` allowances; routers store trait objects).
- Code: thegn-svc (issue/, calendar/, provider.rs, fly/, machine0/, vps/), thegn-media, thegn-host callers.
