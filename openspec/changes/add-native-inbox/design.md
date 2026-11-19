# Design

- Daemon-only: armed by `[daemon] enabled`, `headless_producers`,
  `[inbox] enabled` and one enabled implemented account. The UI never polls.
- Untrusted: every field passes `inbox::sanitize_item`; item text never becomes
  argv, env, path, scope, rule, autonomy or approval.
- Credentials: `token`, `password`, `env` values are secret refs resolved in the
  daemon; literal token/password is refused by `config validate`.
- First poll (or first after a `config_digest` change) is a baseline: at most 20
  newest notified, no rule events. An item leaving and re-entering is new.
- Read-only: `mark_read` is an optional trait op with the caps bit false.
- Calendar is time-keyed with its own sync, so it is not folded in.
