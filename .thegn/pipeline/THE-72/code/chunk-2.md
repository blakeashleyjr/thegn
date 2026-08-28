# THE-72 — chunk 2: surface tracker errors, typed SecretRef tokens, id routing

Read `.thegn/pipeline/THE-72/architect/design.md` §1.7–1.9, §2.4–2.5 first.
This chunk stops a failing tracker from looking like an empty tracker, resolves
account tokens through the typed `SecretRef` vocabulary (including `keyring:`),
and fixes the misleading error on `wt new --from-issue <bare-id>`.

## Files touched (exact)

| path                                       | action                                   |
| ------------------------------------------ | ---------------------------------------- |
| `crates/thegn-svc/src/issue/mod.rs`        | edit                                     |
| `crates/thegn-svc/src/issue/secret.rs`     | **new** (token resolver + hook)          |
| `crates/thegn-host/src/main.rs`            | edit (one install line)                  |
| `crates/thegn-host/src/secret.rs`          | edit (delete a stale caveat)             |
| `crates/thegn-host/src/cmd/issue.rs`       | edit                                     |
| `crates/thegn-host/src/hydrate_tracker.rs` | edit (one `tracing::warn!`)              |
| `config/config.toml.example`               | edit (document the accepted token forms) |

Do **not** touch `crates/thegn-svc/src/issue/linear.rs` or its new siblings.

## Overlap / dependency

**None with chunk 1** — chunk 1 is confined to `issue/linear.rs` +
`issue/linear_schema*.{rs,json}`, and neither chunk reads the other's new
symbols. File-disjoint, parallel-safe, either order.

`issue/mod.rs` gains one `mod secret;` declaration — chunk 1 adds its test
module inside `linear.rs`, not in `mod.rs`, so there is no collision there
either.

## Approach

### A. Typed token resolution (`thegn-svc/src/issue/secret.rs`, new)

`backend_from_account` resolves three provider tokens with
`expand_env_ref(&a.token).unwrap_or_default()` (`mod.rs:157`, `:167`, `:176`).
That handles `env:` / `file:` / bare-literal (`config.rs:82-100`) but **not**
`keyring:` — a `keyring:my-linear` ref is handed to the provider as the literal
API key, which fails as an opaque auth error. (The issue's claim that `file:`
is broken is wrong; `keyring:` is the real gap — see design §1.7, and the
already-documented debt at `thegn-host/src/secret.rs:150-155`.)

New module:

```rust
//! Tracker-account token resolution.
//!
//! Parses the config string once into a typed `SecretRef` (BareAs::Literal —
//! the historic issue/CI-token meaning, secretref.rs:33-42) and resolves it.
//! `keyring:` needs OS credential-store access, which svc cannot link, so the
//! host installs a resolver at startup; without one a `keyring:` ref resolves
//! to None with an actionable warning rather than being sent as a literal key.

/// Install the process's `keyring:` resolver. Idempotent — first call wins.
pub fn install_keyring_resolver(f: fn(&str) -> Option<String>);

pub(crate) fn resolve_account_token(raw: &str, provider: &str) -> Option<String>;
```

- `OnceLock<fn(&str) -> Option<String>>`, exactly the shape of
  `crates/thegn-host/src/forge_handle.rs:17-24` (install-once process global,
  `get()` never panics). A plain `fn` pointer keeps it `Send + Sync` with no
  boxing.
- `resolve_account_token` matches `SecretRef::parse(raw, BareAs::Literal)`:
  - `Env { var }` → `std::env::var`, dropped when empty;
  - `File { path }` → read + trim via `thegn_core::util::expand_tilde`, dropped
    when empty;
  - `Literal(v)` → `v.expose()` (non-empty);
  - `Keyring { account }` → the installed resolver, else
    `tracing::warn!(target: "thegn::secret", provider, ref = %r.audit_name(),
"keyring: tracker token cannot be resolved here — install the host resolver, or use file:/env:")`
    and `None`.
- **Never log the value.** Use `SecretRef::audit_name()`
  (`secretref.rs:159-166`), which is value-free by construction. No `Display`
  on the value exists — keep it that way.
- Unit tests in the module: env / file / literal / empty / `keyring:` with no
  hook installed (⇒ `None`, no panic) and with a hook installed (⇒ its value).
  Use a unique env var name and a temp file, as
  `thegn-host/src/secret.rs:590-613` does.

Then in `mod.rs`: `mod secret;` (pub, so the host can call
`install_keyring_resolver`) and replace the three `expand_env_ref(&a.token)`
calls with `secret::resolve_account_token(&a.token, "<provider>")
.unwrap_or_default()`. Leave `kaneo_stored_token` (`mod.rs:201-210`) on
`expand_env_ref` — it resolves a ref the DB stored, and re-pointing it is a
separate concern.

### B. Host installs the resolver

`crates/thegn-host/src/main.rs`, beside `crate::forge_handle::install(&cfg);`
(`:1045`) — that runs **before** the subcommand match, so it covers every verb
including `daemon` and `serve`:

```rust
// Tracker tokens resolve through the same broker as provider tokens, so a
// `keyring:` ref works for [issues] too (svc cannot link the keyring itself).
thegn_svc::issue::secret::install_keyring_resolver(|r| {
    crate::secret::resolve_for(r, "issue")
});
```

`resolve_for` already emits the value-free audit event
(`thegn-host/src/secret.rs:43-88`). A process that never installs (any unit
test) degrades to today's behaviour minus the bogus literal — the same
fail-safe posture as `publish_background_limits` (`main.rs:1050-1056`).

Then update the now-stale caveat in `crates/thegn-host/src/secret.rs:150-155`
(`store_file`'s doc comment): issue tokens _do_ reach the keyring-capable
broker now. Keep `store_file` itself — CI tokens still need it; just narrow
what the comment claims.

### C. Surface the error on the CLI list path (design §1.8)

`IssueRouter::list_issues` (`mod.rs:290-301`) swallows every per-account error
into a `tracing::warn!` and returns `Ok(all)`. With `THEGN_LOG` unset **no sink
is installed at all** (CLAUDE.md, Performance invariants), so a 400 reaches the
user as `No issues found` and exit 0 — a confident wrong answer on a
user-invoked action.

- **Keep** `list_issues`'s best-effort merge (the background fan-out must not
  blank three good accounts because a fourth failed). Extend its doc comment to
  say so and to point at `list_per_provider` for callers that must report.
- **`cmd/issue.rs::list_tracker_issues`** (`:134-166`) switches to
  `router.list_per_provider(&filter)` (already public, `mod.rs:305-318`):
  - collect `(account, provider, Result)`;
  - for each `Err`, `thegn_core::msg::warn(&format!("{provider}/{account}: {e}"))`
    (`crates/thegn-core/src/msg.rs:60`);
  - if **every** account failed, `msg::die` — a total failure must exit
    non-zero, never print `No issues found`;
  - otherwise merge the `Ok`s and print as today (`--json` unchanged: the JSON
    array still goes to stdout, warnings to stderr, so piping still works).
- **`hydrate_tracker.rs:80-87`**: the error arm `continue`s with no log at all.
  Add one `tracing::warn!(account = %account, provider, error = %e, "tracker
refresh failed")` before the transient check. Panel semantics unchanged —
  a failing account still leaves its prior cache intact.

### D. Bare issue id routing (design §1.9)

`backend_for_id` (`mod.rs:280-286`) splits on `:` and matches the prefix
against `provider_id()`. A bare `THE-72` matches nothing, so
`wt new --from-issue THE-72` dies with `fetch issue THE-72: no issue provider
configured` (`cmd/wt.rs:476-485`) on a machine where a provider _is_
configured.

In `backend_for_id`: if no backend matches the prefix **and exactly one backend
is configured**, return it. Two or more stay strict — guessing across accounts
is worse than an error. Then make the miss message honest: in
`get_issue`/`update_issue`'s `None` arm, return
`IssueError::Api` naming the expected `"<provider>:<key>"` form and the
configured provider ids, instead of the flat `NotConfigured`. Keep
`NotConfigured` for the genuinely-unconfigured router (`self.inner.is_empty()`).

Add unit tests beside the existing `dispatch_by_id_prefix`
(`mod.rs:459-482`): a bare id with one backend routes to it; a bare id with two
backends routes nowhere; an explicit prefix still wins over the fallback.

### E. Document the accepted token forms

`config/config.toml.example:597-616` shows only
`api_key = "env:LINEAR_API_KEY"`. State the four accepted forms for the
`[issues.*]` token fields (and `[[issue_accounts]] token`):
`keyring:<account>`, `env:VAR`, `file:PATH` (`~` expanded), or a bare literal
(deprecated — `thegn secret migrate` moves it out). Keep it to a few comment
lines in the existing voice; do not restructure the section.

## Conventions to honour

- No secret value in any log line, error string, or `Debug` — `audit_name()`
  only.
- New `let _ =` / `.ok()` needs a `// best-effort: <why>` comment
  (ignored-`Result` ratchet).
- `thegn-core` is not touched — no substrate leaks, no coverage-gate exposure.
- `main.rs` is a legacy god-file: add the single install line, nothing else.

## Tests to run (scoped — never a full-workspace gate)

```
just quick thegn-svc
just quick thegn-host
cargo nextest run -p thegn-svc issue
cargo nextest run -p thegn-host secret
```

Do not run `just test`, `just ci`, `just coverage`, or e2e.

## Done criteria

- [ ] `keyring:` tracker tokens resolve through the host broker; with no host
      resolver installed they yield `None` + a value-free warning, never a
      literal `"keyring:…"` API key.
- [ ] `env:` / `file:` / bare-literal tokens resolve exactly as before
      (regression tests prove it).
- [ ] `thegn issue list --status …` against a failing provider prints the
      provider error on stderr and exits non-zero when every account failed —
      never a bare `No issues found`.
- [ ] `--json` output shape unchanged (warnings on stderr only).
- [ ] A tracker refresh failure is logged in `hydrate_tracker`.
- [ ] `wt new --from-issue THE-72` routes to the single configured backend; with
      two backends the error names the `"<provider>:<key>"` form and the
      configured providers.
- [ ] `config/config.toml.example` documents all four token forms; the stale
      caveat in `thegn-host/src/secret.rs` is corrected.
- [ ] `just quick thegn-svc` and `just quick thegn-host` clean.
- [ ] Committed with **exactly** this subject:

```
fix(issues): surface tracker errors, resolve tokens via SecretRef (THE-72)
```
