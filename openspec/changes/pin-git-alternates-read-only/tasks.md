## 1. Pins

- [x] 1.1 Add `objects/info/alternates` and `objects/info/http-alternates` to
      the commit-only/push-lane/full `ro_after` pins; create missing pin
      targets and their parent directory.

## 2. Probe and gate

- [x] 2.1 Extend `GitProbe` with `alternates_nonempty`, `alternates`,
      `objects_symlinks` and `commondir_outside`, read before the pins create
      the empty files.
- [x] 2.2 Add `sandbox_floor::git_integrity_gate` with the refusal and warning
      strings; record a sealed refusal on the spec so `home_gate` refuses.

## 3. Docs

- [x] 3.1 One paragraph on alternates under "Git write policy" in
      `docs/help/sandbox-backends.md`.

## 4. Verification

- [x] 4.1 Unit tests: `alternates_pinned_ro`,
      `sealed_refuses_external_alternates`, `standard_warns_on_alternates`,
      `commondir_outside_refused_sealed`, probe tests.
- [ ] 4.2 Remote build and tests green.
