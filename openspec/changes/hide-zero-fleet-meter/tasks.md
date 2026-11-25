# Tasks

## 1. Implementation

- [x] 1.1 `[bars] fleet_always_show` (default false) in `BarsConfig`
- [x] 1.2 `fleet_widget` hides a zero burn with no unpriced VM and no warning unless the flag is set
- [x] 1.3 Masthead `fleet` arm passes the flag through
- [x] 1.4 Config example, help page `bars`, env-overlay ratchet pin, defaults coverage

## 2. Verification

- [ ] 2.1 Masthead render tests at 200, 120, 80 and 60 columns pass on the remote gate
- [ ] 2.2 Live check: zero-burn fleet hidden, flag on shows `$0.00/h`
