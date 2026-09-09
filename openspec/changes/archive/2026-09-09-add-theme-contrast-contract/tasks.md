# Tasks — theme contrast contract (THE-6)

- [x] Add a pure exhaustive contrast audit over the palette's composed text/
      surface pairs with role-specific floors and deterministic findings.
- [x] Replace the prior narrow copy-legibility test with every-preset coverage.
- [x] Retune light, solarized-light, and any other failing presets until the
      complete built-in set passes the contract.
- [x] Preserve user-override freedom by reporting contrast findings as warnings
      rather than rejecting custom themes.
- [x] Feed the audit into theme-builder feedback through the shared contract.
- [x] Run the recorded core tests/coverage and strict OpenSpec validation; land
      as `c2a72dfc`.

## Validation boundary

No separate historical live eyeball or theme-styled visual-baseline run is
claimed. The accepted requirement is the objective, exhaustive token-pair gate.
