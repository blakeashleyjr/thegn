# Vercel sandbox model credentials, reap and pull-back

## Why

The Vercel sandbox kind (b154) is credential-free, so model-backed agents
cannot log in there. The owner needs an explicit, narrow way to supply a model
login, a way to clean up orphaned sandboxes, and a pull-back marker so lane
results are not forgotten.

## What Changes

- Per-env `model_credentials` = `none` (default) | `upload` | `proxy` | `inject`.
  Only `none` and `upload` are accepted; `proxy` and `inject` are refused with
  their own messages; non-vercel kinds refuse any value but `none`.
- `upload` copies claude/codex login files to tmpfs at each terminal open,
  after egress narrowing, never in capture mode or any read.
- `interactive_host_suffixes` pins the WebSocket host.
- `thegn env reap --provider vercel` (dry run by default).
- An unpulled marker and the `p` panel key.
- OIDC tokens are not refreshed; 429 handling; Vercel cost accrual estimate.

## Impact

- **Roadmap**: J (Remote access), item 765 (THE-2233).
- Affected specs: `sandbox` (remote providers).
- New config keys: `model_credentials`, `interactive_host_suffixes`. No schema
  bump, no new crate.
