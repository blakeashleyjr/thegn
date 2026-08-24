# Add a provider implementation (forge / CI / tracker / calendar / media / …)

The seam shape is `openspec/specs/provider-seams`; the forge is the reference
(`crates/thegn-core/src/forge/`, `crates/thegn-svc/src/forge/`).

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

Out-of-process instead? A plugin can _be_ an issue provider — declare an
`IssueProvider` contribution and answer `provider.call` requests
([plugin.md](plugin.md) step 5); no Rust required.

**Gates:** `kind_coverage` (kind neither built nor reserved), the
`forge-leak` ratchet (a vendor CLI call outside the impl), `config validate
--strict` (reserved kind selected), `thegn doctor` Providers section (probe
present; `test/smoke.sh`), `test/async-trait-ratchet.txt` (empty; any
`#[allow(async_fn_in_trait)]` is a regression), `conformance` tests
(probe shape / account-factory coverage).
