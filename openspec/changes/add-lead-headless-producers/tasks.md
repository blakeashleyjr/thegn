# Tasks

## 1. Producers

- [x] 1.1 Producer owner, arming, leases, idle-exit guard
- [x] 1.2 Tracker, PR poll, mention and PR queue producers in the daemon
- [x] 1.3 UI yields to a held lease
- [x] 1.4 `thegn lead login`, doctor producer status

## 2. Docs

- [x] 2.1 Help pages, CHANGELOG, Lead skill

## 3. Live verification (THE-2208)

- [ ] 3.1 Live run: with no window open, an `issue_matched` rule fires, a UI opened later yields, and killing the daemon lets the UI resume after the TTL

## 4. Amendments (batch 163)

- [x] 4.1 Producer leases are taken over with a compare-and-swap so two daemons never both hold one
- [x] 4.2 A dead holder's lease is taken over after its TTL
- [x] 4.3 The tracker producer keeps a memo of the last refresh so an unchanged repo fires nothing
