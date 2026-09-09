# Tasks — theme builder overlay (THE-7)

- [x] Add overlay state, palette preview, confirm, and cancel behavior.
- [x] Add built-in and user-theme discovery with collision/parse warnings.
- [x] Add per-token color and hue editing with contrast feedback.
- [x] Persist selection and token overrides through comment-preserving config
      edits.
- [x] Add safe named user-theme writes confined to the themes directory.
- [x] Add bounded local Gogh parsing and pure token-palette conversion.
- [x] Add `thegn theme list`, `set`, and `import` through the standard emitter.
- [x] Move scans, imports, and writes into `ThemeStore` background work with
      stale-result handling and a terminal wake.
- [x] Cover palette conversion, invalid input, collision, confinement, reload,
      overlay, and CLI behavior with tests.
- [x] Record accepted v1 exclusions: no export, remote catalog, Base16, GUI, or
      runtime plugin theme surface.
- [x] Reconcile and strictly validate the accepted delta.
