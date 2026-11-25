# Tasks

- [x] Keep Sprites and Vercel `[env.*]` tables in the stable clamp; keep every
      other provider env and `[host.*]` clamped.
- [x] Stop refusing `thegn job` on stable and run the job-ledger startup
      reconcile on both channels.
- [x] Show the panel Environments section on stable; keep Hosts dev-only.
- [x] Update stable-halt and doctor wording that called every provider env
      dev-only.
- [x] Tests: stable clamp keeps exactly Sprites and Vercel; `thegn job` is not
      an experimental verb while `thegn host` still is; the THE-2007 daemon
      refusal no longer fires for Sprites/Vercel; panel section gating.
- [x] Help, README and KNOWN_ISSUES say Sprites and Vercel are supported on
      stable.
- [ ] Paid live run of `test/sprites-live.sh` and `test/vercel-live.sh` on a
      stable binary before the launch.
