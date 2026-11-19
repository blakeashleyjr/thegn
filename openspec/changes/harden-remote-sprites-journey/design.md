# Design

## Decisions

- **Ledger is the proof.** Sprites labels are read-only, so `vps::registry`
  (`provider = "sprites"`) holds a `creating` record before the create call and
  `ready` after. No record means no delete. There is no legacy adoption.
- **Unrecorded destroy.** One read-only GET; 404 means already gone, anything
  else is a refusal.
- **Reap spares young records.** A record under 60 seconds old may belong to a
  create in flight.
- **Hold.** A POSIX `sh` heartbeat registers `thegn-hold-<16 hex>` (5 minute
  expiry), refreshes every 60 seconds and stops at the ceiling
  (`max_lifetime_secs`, else 4 hours; `--hold` capped at 24 hours). Headless
  only. Nothing on the host polls.
- **No background reaper.** Deleting a paid resource automatically is an owner
  call; `env reap` is explicit only.
- **Conformance.** `provider_conformance.rs` with a `suite` entry point, run
  against the sprites mock and the Vercel fake. WS08 extends it.
