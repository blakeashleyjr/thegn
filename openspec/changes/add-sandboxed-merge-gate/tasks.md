# Tasks

## 1. Gate

- [x] 1.1 `[merge_queue] gate_sandbox` key, trusted-config only
- [x] 1.2 Gate, setup and regenerate commands run through the sealed sandbox
- [x] 1.3 Sandbox failure is `GateVerdict::Error`; red log header names the network
- [x] 1.4 Unattended drain never hands a red gate to the fixer agent

## 2. Docs

- [x] 2.1 Help pages (`lead`, `automations`, `sandboxing`), CHANGELOG, Lead skill

## 3. Live verification (THE-2200)

- [ ] 3.1 Live run: an unattended `merge` land through a sealed gate, and a sealed gate that needs the network going red and asking
