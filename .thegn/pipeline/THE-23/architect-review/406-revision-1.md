# THE-23 architect review — revision 1

Issue: THE-23 · branch: `tg/the-23-devcontainer` · review row: 406

---

files: [crates/thegn-host/src/devcontainer_provider.rs, crates/thegn-host/src/handlers/repo_trust.rs, crates/thegn-host/src/agent.rs]
overlaps: []
after: []

---

## Gap

The CLI provider path is not yet inside the same trust and precedence boundary
as the native OCI path.

1. `crates/thegn-host/src/agent.rs:348-372` starts the CLI provider before the
   normal backend resolution and returns
   `SandboxOutcome { spec: None, backend_label: "devcontainer" }`. This path does not check the effective
   sandbox enablement or preserve a user-selected backend/profile/network. For
   example, an explicit `backend = "bwrap"` or `backend = "none"` can still be
   replaced by `devcontainer up`, and the configured hardening profile/network
   is not represented in the provider argv. That contradicts the design's
   trusted-user precedence and can silently weaken the default hardened sandbox.

2. `crates/thegn-host/src/devcontainer_provider.rs:388-397` invokes
   `devcontainer up` with the inherited host environment. The CLI receives the
   raw repository JSON, so `${localEnv:SECRET}` (including in fields thegn
   does not normalize) can be expanded by the vendor CLI from an arbitrary
   host variable. The allowlist in `repo_trust.rs:136-177` only clamps thegn's
   native substitutions; `provider_eligible` does not constrain the environment
   inherited by the raw-file provider.

This is not a style issue: a repo config can cause a trusted devcontainer CLI
to read host secrets or run with weaker isolation than the user's effective
sandbox policy.

## Required correction

Keep the existing native OCI fallback as the authoritative path whenever the
CLI cannot honor the effective sandbox policy. In particular:

- Do not take the provider branch when sandboxing is disabled or when the user
  has selected an explicit backend whose semantics the provider cannot
  preserve. Do not let a provider launch bypass the effective `profile`,
  `network`, and other hardening requirements. Either implement a small,
  explicit provider-capability predicate and test it, or route such cases
  through the existing folded `SandboxSpec`; never infer that `spec: None`
  means the same policy was applied.
- Keep provider selection after the core trust/overlay decision, and preserve
  the existing no-raw-file rule for refused/reserved/unknown fields.
- Ensure the environment of every host-side provider command cannot expose
  variables outside the effective `[sandbox].env_passthrough` set to raw
  devcontainer substitution. A minimal safe implementation may clear the
  command environment and re-add only the provider's required runtime values
  plus the explicitly allowlisted local-env values; do not log values. If the
  provider cannot safely receive a sanitized environment, mark it ineligible
  and use native OCI instead. Ensure the provider exec path does not reintroduce
  the unrestricted inherited environment.
- Preserve the allowlist behavior for both approved and blocked
  `${localEnv:NAME}` substitutions. Blocked names must expand empty and be
  reported by thegn; they must never become available merely because the CLI
  path was selected.

Add focused tests that prove: an explicit backend/hardening policy is not
silently replaced by the CLI provider; a fake `devcontainer` command cannot
observe a non-allowlisted environment variable during `up`/`exec`; and the
native fallback remains selected for ineligible cases. Keep provider command
construction inside `devcontainer_provider.rs`; do not add a backend enum,
database state, control capability, or UI-loop process call.

## Verification

Run the scoped checks after the correction:

```text
RUSTC_WRAPPER= cargo clippy -p thegn-host --bins -- -D warnings
RUSTC_WRAPPER= cargo nextest run -p thegn-host devcontainer doctor switch_cache
RUSTC_WRAPPER= cargo nextest run -p thegn-core devcontainer config::tests::env_overlay
```

Also run `git diff --check`; do not run live provider/e2e or full-workspace
gates for this revision.

Commit subject:

`fix(the-23): keep devcontainer provider inside sandbox trust boundary`
