# Tasks

- [x] Add Unix descriptor-relative exclusive regular-leaf creation and bounded
      write/flush methods without changing existing lock/read behavior.
- [x] Replace the predictable cross-host bundle path with private directory and
      retained-leaf custody, including replacement-preserving cleanup.
- [x] Add the Windows-local create-new, delete-sharing and no-follow seam while
      preserving existing bundle fetch behavior.
- [x] Convert the shipping bundle/fetch fixture to a private owned root and add
      bounded replacement, hostile-leaf canary, write/flush failure, unwind,
      and two-child exclusive-creation coverage.
- [x] Record the supported Unix and Windows custody boundaries and the
      same-UID observation limitation.
- [x] Run root-owned Unix native tests and source/spec/delivery gates.
- [x] Review the Windows exclusive-create/DACL source and explicitly record
      unavailable Windows native proof in the delivery audit.
- [x] Complete adversarial review, minimum scoped native execution and reviewed
      local-main landing. Remote-sync/THE-223 work remains separately owned.
