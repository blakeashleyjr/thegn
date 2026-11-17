# Design

## Decisions

- **One key, default none.** `[env.<name>.provider] model_credentials`. Only the
  user's own config sets it; the wizard never writes a non-`none` value,
  `env create` has no flag, and a repo-local overlay cannot set it.
- **Upload minimum.** claude: `.credentials.json` plus a synthesized
  `.claude.json` of `{"hasCompletedOnboarding":true}` (UNVERIFIED). codex:
  `auth.json`. Nothing else.
- **Placement.** `/dev/shm/thegn-login-<nonce>/<agent>/`, 0700 dirs, 0600
  files, verified with `stat`; refuse if not tmpfs. No disk fallback. Memory only (UNVERIFIED snapshot exclusion).
- **Egress first.** Narrow to model hosts (UNVERIFIED) plus the env's allow
  domains before writing any byte; failure means no upload.
- **Hostile-process paragraph.** A process in the sandbox can read the files
  and use the login for model calls while the sandbox runs; exfiltrate it to
  any allowed host (domain fronting is possible where a CDN accepts it); and
  use the OAuth refresh token after the sandbox is gone until the owner
  revokes it. It cannot reach the Vercel token, git tokens or any other host
  secret, since none are uploaded. A dedicated account is recommended.
- **proxy refused.** Needs a public HTTPS endpoint, Vercel OIDC verification,
  a new auth mode, per-sandbox scoping, revocation on teardown, and cannot
  front subscription logins. Queued separately.
- **inject refused.** Hands the key to Vercel's firewall.
- **Host pin.** `interactive_host_suffixes`, default `vercel.run`, `vercel.sh`,
  `vercel.com`; never `vercel.app`. Refusal falls back to stream mode.
- **OIDC.** No refresh; refuse under 5 minutes left.
- **429.** Honour `Retry-After` <= 20 s on retried operations.
- **Reap.** Dry run by default; `--apply` needs tag proof and no ledger record.
- **Marker.** `$XDG_STATE_HOME/thegn/vercel/unpulled/<name>`, set when a TTY
  session ends, cleared by a successful pull.

## Unverified

Model host lists; claude minimum credential; tmpfs never reaching the auto
snapshot; refresh-token rotation conflict; WebSocket host; OIDC `alg`/`iss`;
per-sandbox session listing.
