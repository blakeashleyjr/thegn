## 1. Config

- [x] 1.1 `GitBackendKind` + `[git] backend`, example, pin 69

## 2. Svc

- [x] 2.1 `GitBackend::glyph_reads` (default + GixGit bridge override); free fn removed; `GitBackend: Probe`
- [x] 2.2 `backend_for` + kind coverage; registry shows selection + CLI write engine

## 3. Host

- [x] 3.1 `git_handle`; installed on both startup paths; 12 hydrate + 2 remote_poll sites; lint guard

## 4. Gate

- [x] 4.1 clippy core/svc/host; core + svc + host suites; lint; openspec validate; fmt
      _(plus THEGN_GIT_BACKEND env knob — the env-overlay gate demanded it.)_
