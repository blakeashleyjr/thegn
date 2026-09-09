# Tasks — external editor handoff (THE-17)

- [x] Add project/file/line/column editor target planning in core.
- [x] Preserve configured-editor resolution and external-versus-pane placement.
- [x] Route sidebar, diff, PR, and palette handoffs through the shared seam.
- [x] Add the cataloged `editor.open` operation to public transport projections.
- [x] Decode requests strictly and enforce worktree containment.
- [x] Deliver daemon requests through expiring async intents without UI-loop I/O.
- [x] Cover project targets, file positions, invalid paths, expiry, placement,
      and public transport behavior with tests.
- [x] Remove unimplemented URL-handler, launcher, and IDE-extension promises from
      this delivered change; track the inbound decision in THE-104.
- [x] Reconcile and strictly validate the accepted delta.
