# Add a provider implementation (forge / CI / tracker / calendar / media / …)

The seam shape is `openspec/specs/provider-seams`; the forge is the reference
(`crates/thegn-core/src/forge/`, `crates/thegn-svc/src/forge/`).

For the account-shaped issue tracker seam, use the focused
[tracker-provider recipe](tracker-provider.md). It documents the live
`IssueBackend`/`IssueCaps` contract, plugin bridge, offline doctor probe, and
CLI-backed-provider boundary.

1. **Kind**: add the value to the seam's `config_enum!` (`ForgeKind`,
   `CiProviderKind`, …). If you are landing the config name before the
   implementation, mark it `reserved`. Account-shaped seams (issues,
   calendar) skip this — their `[[…_accounts]]` list is open; the factory
   (`backend_from_account`) is the registration point.
2. **Implement the trait** in a new file under the seam's module:
   `id()`, `caps()` (only the ops you really do), the required ops, and
   `Probe` (cheap and offline — a `which`, an env check; never a network
   round-trip). Vendor CLIs are invoked only inside this file. Sync seams
   use plain `&self`; async seams use `fn m<'a>(…) -> BoxFuture<'a, R>` with
   the body in `Box::pin(async move { … })` — **never `async fn` in the
   trait, never a delegation enum** (dispatch is `Box<dyn T>`).
3. **Factory arm**: `forge_for_kind` / `client_for_system` / the seam's
   router returns your impl for the kind (and `None` for reserved ones).
4. **Errors** map onto the seam's `SeamError` classes: "this layer can't"
   (`Unsupported`, `NotInstalled`, `NotConfigured`) falls through a ladder;
   `Auth`/`NotFound`/`Transient` are final.
5. **Config + docs**: `config/config.toml.example` for the kind and any
   sub-table (no sub-table for reserved kinds); `docs/help/` if user-visible.
6. **Tests**: a unit test per op you implement (parsers in core are
   coverage-gated), the seam's `kind_coverage` call stays green, and the
   cross-seam shape suite (`thegn_svc::conformance`) already covers your
   probe — a malformed report or a factory/kind mismatch fails it.

## The `SecretStore` seam (credential backends)

The credential broker (THE-66) is a seam too: `thegn_core::secret_store`
declares the object-safe `SecretStore` trait, the `SecretBackendKind`
`config_enum!` (`keyring`/`file`/`env` implemented, `exec` reserved), and the
classed `SecretError` (`unavailable` falls through a ladder; `denied`/`not_found`
are final). The backends live host-side (`crates/thegn-host/src/secret.rs`:
`KeyringStore` / `FileStore` / `EnvStore`), because the keyring FFI and file I/O
need a substrate `thegn-core` must not depend on — the same core-trait /
host-impl split as every other seam. `thegn doctor`'s `Secrets` section renders
one probe per backend (`crate::secret::probes()`), with `exec` shown reserved.
To add a backend: add its `SecretBackendKind` value, implement `SecretStore` +
`Probe` host-side, and add its probe row. Resolution flows through the one
chokepoint `secret::resolve_ref_for`, which emits the value-free
`thegn::secret::audit` event — never resolve a `SecretRef` any other way.

Out-of-process instead? A plugin can _be_ an issue provider — declare an
`IssueProvider` contribution and answer `provider.call` requests
([plugin.md](plugin.md) step 5); no Rust required.

**Gates:** `kind_coverage` (kind neither built nor reserved), the
`forge-leak` ratchet (a vendor CLI call outside the impl), `config validate
--strict` (reserved kind selected), `thegn doctor` Providers section (probe
present; `test/smoke.sh`), `test/async-trait-ratchet.txt` (empty; any
`#[allow(async_fn_in_trait)]` is a regression), `conformance` tests
(probe shape / account-factory coverage).
