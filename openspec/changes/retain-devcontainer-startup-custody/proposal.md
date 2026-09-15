# Retain devcontainer startup custody and halt uncertain fallback

THE-639: `devcontainer up` waited for exit before draining stderr, then read without a byte bound; timeout could block during cleanup. The agent treated every failure as permission to continue to OCI, even after the CLI may have created a resource.

Reserve one private startup owner before provider construction, drain at most 2 MiB of stderr concurrently, preserve stdout-null/exit-status success, and bound the caller by the existing ten-minute deadline. Retain unresolved child, pipe, snapshot and handoff ownership. Only proven pre-spawn failures may continue to OCI; uncertain effects halt the launch through the existing error path.

No new backend, UI/config schema, process signals, resource deletion or durable ledger. Cross-controller/restart prevention and authoritative resource identity remain outside THE-639 / with THE-520. This work depends on the reviewed private THE-633 source; that dependency is not itself a claim of landing.
