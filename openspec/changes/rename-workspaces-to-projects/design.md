# Design — workspaces → projects

## The scope decision, argued

Two coherent end states exist; half-measures between them are the only wrong
answer.

**A. UI-label rename (chosen).** Users see "project" everywhere; config
accepts `project` spellings; machine identifiers stay `workspace`.

**B. Full rename.** Every identifier flips: DB table, types, action ids,
JSON fields, spec capability, config keys with aliases.

Cost table for B, from the survey:

| surface                       | cost                                                                                            |
| ----------------------------- | ----------------------------------------------------------------------------------------------- |
| DB `workspaces` table         | schema migration; collides with in-flight `add-workspace-zones` (v33 adds `workspaces.zone_id`) |
| action ids (`new-workspace`…) | user `[keybinds]` tables break; keymap alias layer needed forever                               |
| control API / `--json` fields | external consumers break; versioning or dual-emit needed                                        |
| `workspace` spec capability   | 65-cap list + cross-references in 40+ in-flight change folders                                  |
| `[workspace.<slug>]` config   | same alias layer A needs anyway                                                                 |
| core types / module names     | mechanical but touches nearly every crate; churns every open branch                             |

The decisive point: **B's alias layers mean two vocabularies survive
anyway.** Given that, the only question is where the seam sits. A puts it at
the UI boundary — one sentence explains it ("the UI says project; config and
APIs use the internal term workspace, and both spellings work in config").
B puts the seam _inside_ the machine surface (some ids renamed, aliases for
the rest), which is strictly harder to document and to keep ratcheted. The
repo already runs this pattern successfully: "worktree" internally, "tab" in
the UI; `wt` as the CLI namespace.

Do-nothing was also considered: rejected — the issue is a product-naming
decision by the product owner, and the tracker-vocabulary collision
("workspace" meaning Linear/Kaneo workspace _and_ thegn workspace in one
config file) is a real, current confusion that the rename dissolves.

## The new collision, handled

Renaming creates one new overlap: trackers also have **projects**
(`[[tracker]]` `project_id` — "restrict to one project"). Rule: in any UI
string within a tracker context (issues panel, tracker config validation
messages), the foreign concept is qualified — "Linear project", "tracker
workspace" — and the bare word "project" always means the thegn concept.
The config keys under `[[tracker]]`/accounts are the tracker's own nouns and
are not touched. This rule goes in the help page for the panel's Issues
section and is applied during the string sweep.

## Config aliasing mechanics

- Serde aliases on the existing fields: `projects_dir` ⇄ `workspaces_dir`,
  `[project.<slug>]` ⇄ `[workspace.<slug>]`, `confirm_delete_project` ⇄
  `confirm_delete_workspace`, `sidebar_project_sort` ⇄
  `sidebar_workspace_sort`. The struct field names (the schema) stay
  `workspace*`, so "the Rust structs are the schema" holds and the
  home-manager module derives unchanged; the alias is deserialize-side.
- Both spellings present: the `project` spelling wins (it is the documented
  canonical; last-writer ambiguity is banned), and `thegn config validate`
  warns with both locations. Strict validation's unknown-key report gains
  did-you-mean entries for near-miss `project*` keys.
- `config.toml.example` flips its spellings and documents the accepted
  aliases at the top of `[ui]` — satisfying the "every key is documented"
  requirement for both spellings via one entry each.
- No environment-variable rename (none carries "workspace" today — verified;
  `THEGN_*` names are untouched).

## Help-corpus mechanics

- Filenames and page ids stay (`workspaces-and-worktrees.md` keeps its id;
  its _title_ becomes "Projects and worktrees") so `zone:*`/`panel:*` context
  mappings, `include_str!` embeds, and the context ratchet don't churn. A
  rename of the file is cosmetic churn with ratchet/regeneration risk and is
  deliberately not done.
- The prose ratchet requires each page to mention what it claims by chord,
  id, or a distinctive label word. Labels change → the sweep must update
  prose in the same change; all three help ratchet files may only shrink.
- The keybindings and config-reference pages are generated (from
  `keymap_merge::collect` and the schema) — never hand-edited; they follow
  the label/doc changes automatically, and their tests keep them honest.

## What is explicitly NOT renamed (the contract)

DB tables/columns; `thegn_core`/host type, module and function names; action
ids and any id persisted in `ui_state`; control API paths, gRPC, MCP tool
names, capability-catalog ids (none says "workspace" today — keep it that
way for new rows too... new rows use the internal term); `--json` field
names; the `workspace` openspec capability and existing change folders;
`docs/ARCHITECTURE.md` and code comments (machine-facing docs use the
machine term, with one note that the UI word is "project").

## Security

None meaningful: string and parse-layer changes only; no new write surface,
no credential handling, no sandbox implication. The one care point is the
config alias precedence rule being deterministic (spec'd above) so a
malicious/duplicate config cannot smuggle a second value past review — the
explicit `project` spelling always wins and validation names both sites.

## Open questions

- Should the `zone` CLI's user prose say "project groups" or keep "zones" as
  its own brand? (Zones group projects; the noun "zone" itself is untouched.)
- Whether `add-localization`'s string catalog lands first; if so this change
  becomes largely a catalog edit and the sweep shrinks — sequencing to be
  decided by the audit phase.
- Whether the binary's `--help` top-line ("workspace shell") flips now or
  with a broader brand pass.
