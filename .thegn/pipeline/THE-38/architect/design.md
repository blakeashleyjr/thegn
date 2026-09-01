# THE-38 architecture design: configuration audit

## Decision

Keep the configuration contract deliberately asymmetric:

| Surface                                                                       | Accepted representation today                                                                 | Decision                                      |
| ----------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- | --------------------------------------------- |
| Built-in defaults                                                             | Rust defaults                                                                                 | Unchanged                                     |
| User config and `--config`                                                    | TOML, regardless of the supplied path extension                                               | TOML only                                     |
| External active profile overlay                                               | `profiles/<name>/config.toml`                                                                 | TOML only                                     |
| Embedded profiles, keybinds, theme, actions, and all ordinary config sections | Tables/arrays in the same TOML `Config` document                                              | TOML only                                     |
| `--set`                                                                       | Scalar values, or bracket/brace TOML fragments for arrays/tables                              | Not a document format                         |
| `THEGN_*` overlays                                                            | In-memory strings parsed as bools, numbers, enums, lists, and paths                           | Not a document format                         |
| Repo-local overlay                                                            | `.thegn.toml`, `.thegn.yaml`, `.thegn.yml`, or `.thegn.json`; TOML wins, then YAML, YML, JSON | Keep tri-format support at the untrusted edge |

JSON/YAML readers for the trusted config are not warranted. `Config::try_load_layered`
reads the selected file with `toml::from_str` (`crates/thegn-core/src/config.rs:6070-6093`),
profile files are explicitly named `config.toml` and merged through TOML
(`crates/thegn-core/src/config.rs:6116-6125,6165-6188`), and writes use
comment-preserving `toml_edit` (`crates/thegn-core/src/config_write.rs:1-14,143-196`).
The example-coverage test scans TOML and parses it as `Config`
(`crates/thegn-core/tests/config_example.rs:1-13,75-125`), while the runtime
reference is generated from that example (`crates/thegn-core/src/help/config_ref.rs:1-8,53-157`).
Adding trusted JSON/YAML would multiply read/write/live-reload/example/home-manager
paths without a demonstrated user need. The existing repo-local formats meet the
separate need for checked-in, ecosystem-friendly files; they must not become a
reason to weaken the one-user-file story.

This preserves the architecture rule in `docs/ARCHITECTURE.md:201-214`: Rust
structs remain the schema, the example documents every key, and loading remains
tolerant while explicit validation reports problems. It also avoids adding a
format selector or another vendor-shaped provider seam.

## Audit findings and evidence

### Formats and layering

- The main user file defaults to `$XDG_CONFIG_HOME/thegn/config.toml`; an
  explicit `--config` changes the path, not the parser
  (`crates/thegn-core/src/config.rs:6070-6093`). It is TOML-only.
- The named profile file is also TOML-only. The selected profile is loaded as
  a TOML overlay before environment and `--set` overlays
  (`crates/thegn-core/src/config.rs:6116-6125,6165-6188`). A profile can also
  be embedded under `[profiles.<name>]` in the main TOML document
  (`crates/thegn-core/src/config.rs:2210-2248`).
- Keymaps are not a second thegn file format. `[keybinds]`, its mode tables,
  and `[[actions]]` are fields in `Config`; `thegn keys validate` validates
  chords (`crates/thegn-core/src/keymap.rs:1-13`). The repository's
  `config/yazi/keymap.toml` is Yazi configuration, not read by thegn.
- Theme is likewise `[theme]` in the TOML config; the schema-backed
  `ThemeConfig` is defined at `crates/thegn-core/src/config.rs:2607-2645`.
  Environment variables can overlay selected scalar theme values, but do not
  introduce a theme document format.
- `--set` is applied after environment overlays. It parses bracket/brace
  fragments as TOML and otherwise coerces bools, integers, and strings; dotted
  path/type failures are handled by `apply_override_str`
  (`crates/thegn-core/src/config.rs:6127-6139,6191-6268`).
- `THEGN_*` is an in-memory scalar/list overlay. The parser and its coverage
  are in `crates/thegn-core/src/config.rs:5706-6060`; the ratchet requires
  every shallow key to have a knob or an explicit exception
  (`crates/thegn-core/tests/env_overlay_coverage.rs:1-5,25-111`).
- Repo-local files are deliberately different: `load_repo_overlay` checks
  `.thegn.toml`, `.thegn.yaml`, `.thegn.yml`, and `.thegn.json` in that order
  (`crates/thegn-core/src/config.rs:6921-6949`). The actual schema is broader
  than the draft claimed: `RepoConfigFile` contains sandbox, keybinds,
  notifications, issues, an `env` selector, and metrics detection/refusal
  (`crates/thegn-core/src/config.rs:4365-4440`). Existing tests prove that all
  three parsers agree for equivalent overlays
  (`crates/thegn-core/src/config_tests.rs:720-768`).

Therefore the answer is consistent by trust tier, not by one universal file
extension: trusted/user-authored configuration is TOML; untrusted repo policy
has the existing tri-format exception; overrides are values rather than files.
The current inconsistency is documentation and validation coverage, not a need
for more trusted readers.

### Example and runtime reference

`config/config.toml.example` is hand-authored, not generated. That is
intentional: the schema walker checks every `Config` section/key, the file must
deserialize as `Config`, and the validator must accept it
(`crates/thegn-core/tests/config_example.rs:20-125`; the clean validation test
is in `crates/thegn-core/src/config_validate.rs:849-858`). The home-manager
drift gate independently checks rendered keys and enum values
(`crates/thegn-core/tests/hm_module_drift.rs:168-215`). No example rewrite is
required and no new key is introduced.

The in-app `config-reference` page is generated at runtime from the example and
is registered beside generated keybindings (`crates/thegn-host/src/help/pages.rs:1-73`).
It covers the schema keys that the example-coverage gate requires and emits the
example's values/comments. The current wording says “every key with its
default” (`crates/thegn-core/src/help/config_ref.rs:117-122`), but no test
compares example values with `Config::default`; the generator test only checks
that the real example yields a valid, nontrivial page
(`crates/thegn-core/src/help/config_ref.rs:282-302`), and host tests spot-check
four tables (`crates/thegn-host/src/help/pages.rs:173-188`). The change should
correct that claim to “every documented key with its example value” and add a
schema-derived generated-reference coverage assertion. This closes reference
drift without pretending illustrative example values are code defaults.

### Diagnostics

- `config validate` currently reads only the main file and reports raw
  validator strings without a file prefix (`crates/thegn-host/src/cmd/config.rs:338-368`).
  There is no `--strict`; `Action::Validate` has no arguments
  (`crates/thegn-host/src/cmd/config.rs:12-47`). The validator gives useful
  unknown-key hints (`crates/thegn-core/src/config_validate.rs:267-357,417-445`)
  and TOML syntax errors, but generic serde type failures do not consistently
  include a dotted key (`crates/thegn-core/src/config_validate.rs:33-97`).
- `config get` correctly distinguishes effective values and unknown dotted
  paths, but its errors only say `unknown config key: ...` and omit the source
  path (`crates/thegn-host/src/cmd/config.rs:286-305`).
- `config set` already validates before committing and rolls back on a new
  parse/type/enum error; its messages name the requested key but not the file
  for validation failures (`crates/thegn-host/src/cmd/config.rs:49-120`).
- `doctor` currently reports theme modes and capability/provider health but no
  configuration path or validation count (`crates/thegn-host/src/cmd/doctor.rs:1065-1128,1249-1293`).

## Concrete implementation

### Core: one format-neutral validation substrate

Create `crates/thegn-core/src/config_repo.rs` rather than adding more branches
to the already large `config.rs`. Move or re-export the repo candidate
discovery, `RepoConfigFile`, format enum, and parse helpers there. Expose a
pure API that accepts `(body, OverlayFormat)` and returns structured or
path-bearing diagnostics for a repo overlay. Parse TOML/YAML/JSON into one
format-neutral value, then run the same schema walk used by `Config`; keep the
existing `validate_str` entry point and its `Config` behavior.

The shared walk must report:

- syntax errors with the source format;
- unknown dotted keys with the existing nearest-key hint;
- type errors as `path: expected <type>, got <type>`;
- map-valued tables (environment names, action/keybind maps, and other
  intentionally dynamic maps) without rejecting arbitrary map keys;
- legacy keys without duplicate noise.

Repo validation must use the real `RepoConfigFile` schema, including the
metrics detection-only table. It validates the selected winner only, while
candidate discovery reports every shadowed existing path. The warning names
the winner and ignored paths, never contents, and is deduplicated for one load
so repeated effective-field reads do not spam the terminal. Loading remains
tolerant: malformed/unknown repo values still fall back as today.

Add unit tests in the core crate for each format's valid document, syntax error,
unknown top-level key, nested typo with hint, and type error. Add a two-candidate
shadow-warning test. Add a generated-reference test that derives the expected
documented key set from the schema/example and fails if the generated page
drops a key. Do not add a default-value comparison gate.

### Host: layer validation and health reporting

Add `--repo <path>` to `config validate`. Use a host-side layer collector (a
new `crates/thegn-host/src/cmd/config_health.rs`) so file I/O and path discovery
stay at the edge. It validates, in order: selected main TOML, active external
profile TOML, then the selected repo overlay when cwd or `--repo` identifies a
repo. Missing optional layers are silent. Every diagnostic is prefixed with
the path that owns it; any problem makes the command non-zero. The selected
repo format is discovered by the core candidate API, and the profile path is
the active profile path already computed by core.

Make `config get` and `config set` include the effective config path as context
while preserving real typed JSON output and atomic rollback. Do not claim that
an effective key came from a particular layer unless the command has an actual
provenance result.

Make doctor consume this same collector. Add a text line and JSON object with
the main path, profile/repo layer paths when present, problem counts, and the
follow-up command `thegn config validate`. Doctor remains diagnostic-only and
does not change its exit policy. It is a synchronous one-shot CLI path, so
these reads do not enter the compositor/event loop.

Adding `--repo` is a value-taking CLI slot. Register it in the single catalog
as `SourceKind::Structural` (path completion owned by clap) in
`crates/thegn-core/src/completion/catalog.rs`; do not edit the completion-slot
ratchet allowlist. There is no new control capability, so the control-schema
snapshot is unchanged.

### Documentation and OpenSpec

Update §7 of `docs/ARCHITECTURE.md` to state the trust-tier format contract,
profile layer, actual repo tables, tolerant load/strict validation, and the
existing gates. Update `docs/help/configuration.md` to remove every nonexistent
`--strict` claim, describe the real order and repo precedence/shadow warning,
and describe the reference as example-value documentation.

Synchronize all four files in the existing change
`openspec/changes/align-config-formats-and-validation/`: proposal, design,
tasks, and `specs/config/spec.md`. Prune claims already satisfied by the
branch (tri-format parsing, example coverage, generated registration, profile
loading, and home-manager drift) and make the remaining requirements match
the concrete APIs above. Do not edit canonical `openspec/specs/config/spec.md`
until implementation is landed and the change is ready to archive.

## Ratchets and invariants

- `thegn-core` remains substrate-free; parsing/validation APIs are pure and
  unit-tested. File discovery and diagnostics stay in host/core edges as
  appropriate. No blocking work is added to the event loop.
- No new config key means `env_overlay_coverage`, `hm_module_drift`, the
  `config_enum` count, and `config/config.toml.example` need no new entries;
  they still run in the same implementation chunk.
- No new action/help page means the three help ratchets and registration gate
  remain unchanged; the existing generated config-reference tests are
  strengthened instead.
- No control/catalog capability is added. Only the completion value slot for
  `config validate --repo` is cataloged, with the completion-slot ratchet
  unchanged.
- Do not grow `doctor.rs` or `config.rs` into god files; use the new
  `config_health.rs` and `config_repo.rs` modules. Preserve trust clamping and
  all existing repo security behavior.

## Chunk order

1. Core validation, repo format substrate, shadow detection, and generated
   reference coverage.
2. Host `config validate`, `config get/set` context, doctor health, and the
   completion catalog entry. This depends on chunk 1's public core API and
   therefore runs serially after it; the files do not overlap.
3. Architecture/help/OpenSpec synchronization. It is file-disjoint and can
   run in parallel with code work, though its wording describes the completed
   contract.
