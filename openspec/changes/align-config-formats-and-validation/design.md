# Design — settle config formats, close validation blind spots

## Context

The config surface is unusually well-gated already: the Rust structs are the
schema (`schemars`), strict validation walks the raw TOML in lockstep with that
schema (`config_validate::validate_str` — unknown keys with nearest-key hints,
`config_enum!` values, template placeholders, tz names), the example file is
coverage-gated in both directions, and the help page is generated from the
example. The branch already has tri-format repo parsing, profile loading,
home-manager drift checks, and generated-page registration. The audit adds
only the missing shared overlay walk, layer aggregation, shadow warning,
diagnostic context, and generated-reference key assertion.

## Format decision (THE-38's first question)

| Layer                              | Trust                | Format(s)                | Why                                                                  |
| ---------------------------------- | -------------------- | ------------------------ | -------------------------------------------------------------------- |
| defaults                           | code                 | —                        |                                                                      |
| `config.toml` / `--config`         | user                 | TOML                     | one hand-edited file; the whole gate + write chain is TOML-native    |
| `profiles/<name>/config.toml`      | user                 | TOML                     | same file shape, same chain                                          |
| `THEGN_*` env / `--set`            | user                 | scalars / TOML fragments | `coerce_override_value` already parses bracketed TOML                |
| repo `.thegn.{toml,yaml,yml,json}` | **repo (untrusted)** | TOML, YAML, JSON         | checked into other people's repos; meets their ecosystem where it is |

The asymmetry is a feature: format tolerance belongs at the layer whose author
is _not_ the thegn user. Widening the trusted layers to JSON/YAML would need a
multi-format `config set` (comment-preserving editing exists only for TOML), a
multi-format example gate, multi-format live-reload watching, and hm-module
changes — all to serve a file the user writes once. Rejected.

## Validation across layers

`validate_str` stays as-is for `Config`-shaped documents. Added in a new
`thegn-core` sibling module plus the shared validator (pure, unit-testable):

- `validate_repo_overlay_str(body: &str, format: OverlayFormat) -> Vec<String>`
  — parses the body in its format to a format-neutral value
  (`serde_json::Value` via the format adapters), then runs the same schema walk
  against `schema_for!(RepoConfigFile)`. `RepoConfigFile` already derives
  `JsonSchema`; expose a validation entry point rather than its internal
  fields. The walk takes the schema root as a parameter instead of hard-coding
  the `Config` schema.
- The nearest-key hint, dotted type diagnostics, and map-valued-table tolerance
  behave identically — they are properties of the walk, not of the `Config`
  schema.

Host side, `cmd/config.rs::validate` becomes a loop over located layers:

1. main file (existing behaviour, message format unchanged for it),
2. `Config::profile_overlay_path` when a non-default profile is active
   (validated as a `Config`-shaped overlay — it is a full `Config` overlay),
3. the repo overlay found from cwd (or `--repo <path>`), when present.

Each problem line is prefixed `<path>: ` so three files' reports don't blur;
type failures include the dotted key. Exit is non-zero when any layer has
problems; a missing layer is skipped silently (absence is normal, not a
warning). `config get/set` add the effective config path as context without
inventing key provenance.

## Shadowed-overlay warning

`load_repo_overlay` / `repo_overlay_parse_error` currently `return` on the
first existing extension. The new candidate seam records which candidates
exist; when >1, `config_warn` names the winner and ignored files once per load.
Warning text contains only file _paths_ (never file contents — the files are
untrusted). The precedence order itself does not change.

## Doctor line

`cmd/doctor.rs` adds to the text report and the JSON document:

```
"config_health": { "config": {"path": "...", "problems": <n>},
                   "profile": {"path": "...", "problems": <n>} | null,
                   "repo_overlay": {"path": "...", "problems": <n>} | null,
                   "validate": "thegn config validate" }
```

Doctor reuses the same core validation functions; no second policy. It reads
files only — no blocking additions to any event loop (doctor is a one-shot CLI
command, off the compositor entirely). Render damage channels: none touched.

## Docs

`docs/help/configuration.md`: reconcile the two layer paragraphs (real table
set: sandbox — clamped —, keybinds, notifications, issues, `env` selector,
and metrics detection/refusal), drop `--strict` from all mentions, add
multi-format precedence plus the shadow warning, and describe example values
instead of code-default parity. The config-reference page remains generated;
its key-coverage test is strengthened. Help ratchet: no new action ids, no
new pages — the existing `configuration` page keeps its registration.

## Alternatives considered

- **Add a real `--strict` flag (lenient by default)** to match the docs
  instead of fixing the docs: rejected — `validate` exists to be the strict
  check (lenient behaviour is what every _load_ already does); a lenient
  validate validates nothing.
- **Validate overlays on load** (warn at startup rather than via `validate`):
  rejected for scope — load-time is tolerant by contract ("a launch is never
  blocked by configuration"), and unknown-key scanning on every launch adds
  cost to the startup path for a check the user runs deliberately. Doctor is
  the middle ground.
- **A `[config] format` knob or auto-detected `config.{yaml,json}`**: see
  proposal Non-goals.

## Security

- **No new write surface.** All additions are read-and-report.
- **Untrusted input**: repo overlays are attacker-authored. Validation output
  echoes key _paths_ and nearest-hint suggestions derived from the schema, not
  raw file content; parse-error strings from serde are already surfaced today
  via `repo_overlay_parse_error` and are length-bounded by the existing
  `config_warn` path. The shadow warning prints paths only.
- **No credential handling**: no keys in scope carry secrets; `SecretRef`
  handling is unchanged.
- **Sandbox implications**: none — the trust clamp
  (`add-config-trust-resolution`) is untouched; this change never widens what
  a repo overlay can set, it only reports on the files.
- **Doctor JSON** may be piped to other tools; it includes paths and counts,
  no file contents.

## Open questions

- Defaults-accuracy gate for the example's commented `# key = value` lines:
  worth a heuristic pass (only lines whose value parses as the field's type
  and differs from the schema default)? Deferred — noise risk until measured.
- Should `config validate` also name layers it _skipped_ (no profile overlay,
  no repo overlay) for discoverability? No: quiet absence is the Unix contract;
  doctor reports only layers that exist.
