# Tasks

## 1. Tour

- [x] 1.1 `welcome.rs`: chapter state machine, skip/back/skip-all, pending writes
- [x] 1.2 Workspace diagram and key drill on a shared `Mini` model
- [x] 1.3 Look chapter: live theme preview and keymap preset
- [x] 1.4 Sandbox chapter: tier table, support-report probe, `r` re-check
- [x] 1.5 Agents chapter: off-loop harness PATH probe, installed-only pick
- [x] 1.6 Remote chapter: explanation and later-setup commands only
- [x] 1.7 Finish chapter: diff of pending writes, three next steps, `a` hand-off

## 2. Wiring

- [x] 2.1 `OnboardingUi.tour`; first run opens the tour, palette row opens it
- [x] 2.2 Off-loop config writes with a toast result
- [x] 2.3 `config_write::promote_agent`

## 3. Docs and tests

- [x] 3.1 `docs/help/onboarding.md`; update getting-started and command-palette
- [x] 3.2 Unit tests: state machine, writes, drill matching, agent writer
- [x] 3.3 Muse spec `35-onboarding.yaml`: happy path and skip-all
- [ ] 3.4 Compile, `cargo nextest run -p thegn-host welcome`, and the muse spec on the remote builder
- [ ] 3.5 Manual pass at 80x24, `NO_COLOR=1`, and `[theme] glyphs = ascii`
