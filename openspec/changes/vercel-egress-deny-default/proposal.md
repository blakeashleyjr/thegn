## Why

A live run showed `curl example.com` succeeding inside a Vercel sandbox: the
default was allow-all. Code an agent runs in a sandbox should not reach the
network unless the user chose that (THE-2234 step 6b).

## What Changes

- Vercel sandboxes are created with a deny-all network policy and the policy is
  re-applied on every resume.
- `[env.<n>.provider] egress = deny | registries | allow-all` (default `deny`)
  and `egress_allow` (host patterns) pick the policy.
- The new-environment wizard and `thegn env create --egress` offer the choice;
  allow-all is never preselected.
