# THE-38 architect revision 1

## Finding 1 — repo metrics refusal is absent from config health

- **Location:** `crates/thegn-host/src/cmd/config_health.rs:151-165`
- **Problem:** `collect_repo` validates the selected `.thegn.*` document and
  reports shadowing, but never invokes the existing
  `Config::repo_command_collector_warnings` / equivalent core refusal path.
  A repo overlay containing `[[metrics.targets]] kind = "command"` therefore
  passes `thegn config validate` and contributes no doctor warning, even though
  the loader refuses that command collector to prevent repo-supplied command
  execution. This contradicts the branch's documented metrics
  detection/refusal contract and the design's requirement to preserve it.
- **Expected fix:** expose a format/body-based core helper (or an equivalent
  structured diagnostic) that applies the existing command-collector refusal
  to the already-discovered selected candidate, and add its path-prefixed
  warning to `ConfigHealth` for both `config validate` and doctor. Do not make
  it a strict problem and do not reread the overlay through a second discovery
  path. Add a regression test covering a selected repo candidate with a
  command collector and asserting the warning names the target while no
  command is executed.

## Finding 2 — unreadable repo candidates disappear from validation

- **Location:** `crates/thegn-core/src/config_repo.rs:152-167`
- **Problem:** discovery silently drops any candidate for which
  `read_to_string` fails. Consequently an existing but unreadable higher-
  precedence `.thegn.toml` can be omitted from the shadow set while a lower-
  precedence YAML/JSON file is selected, with no path diagnostic. The design
  calls for all existing candidates to be surfaced by shadow/health reporting;
  `config validate` must not present a lower file as the complete audit of a
  repo whose higher candidate cannot be read.
- **Expected fix:** retain existing candidate paths and their read failures in
  the discovery result (or add an explicit discovery diagnostic), report the
  unreadable path in host health, and make the selection/readability semantics
  explicit. Preserve tolerant launch behavior and never include file contents
  in the diagnostic.
