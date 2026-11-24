# Recipes

This directory is the one home for thegn recipes: ready-made definitions a user can copy into their own
config, and (for sandbox backends only) definitions thegn ships as named backends. It is part of the
sandbox + remote programme; see `CONTRIBUTING.md`, "Programme conventions (sandbox + remote, 2026-11)".

`config/` is already a root of the nix source filter (`nix/source.nix`), so files here are visible to
package builds without any change to that filter. Never create a top-level `recipes/` directory: it would be
a second home for config data and is outside the filter.

## Layout

```text
config/recipes/README.md        this file: layout, header, tiers, owners
config/recipes/sandbox/         sandbox backend recipes     (<name>.toml)
config/recipes/provider/        env provider recipes        (<name>.toml)
config/recipes/connector/       remote connector recipes    (<name>.toml)
```

A recipe is one TOML file, `<name>.toml`, in the directory of its kind. The `.keep` files hold the empty
directories in git until the first recipe lands. Nothing in this directory is embedded in the binary or read
by a test of a recipe yet: embedding, the generated `INDEX.md` and the CLI verbs
(`thegn {sandbox,provider,connector} recipe list|show|add|diff`) arrive with the owner items below.

## Header

Every recipe starts with a `[recipe]` table, the same for all three kinds. Unknown keys inside that table
are rejected. The rest of the file holds the tables of its own kind (for example `[sandbox]`), which the
kind's owner defines; nothing outside `[recipe]` is part of the header.

| Key              | Type                                                       | Meaning                                                                                                     |
| ---------------- | ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `kind`           | `"sandbox"`, `"provider"` or `"connector"`                 | Which directory the file belongs in.                                                                        |
| `tier`           | `"documented-only"`, `"fixture-tested"` or `"live-tested"` | How much evidence backs the recipe; see Tiers.                                                              |
| `live_tested_on` | list of strings                                            | `os/arch@date` entries, for example `"linux/x86_64@2026-11-20"`. Required (at least one) for `live-tested`. |
| `revision`       | string                                                     | `YYYY-MM-DD.N`: the date of the last edit and a counter for that date.                                      |
| `last_verified`  | string, optional                                           | `YYYY-MM-DD` of the last check against the real tool.                                                       |
| `spikes`         | list of strings                                            | Ids of the spike decision records the recipe relies on.                                                     |
| `platforms`      | list of strings                                            | Platforms the recipe is meant for, for example `"linux"`, `"macos"`.                                        |
| `register`       | `"template"` (default) or `"shipped"`                      | See Register.                                                                                               |

Example header:

```toml
[recipe]
kind = "sandbox"
tier = "fixture-tested"
revision = "2026-11-20.1"
platforms = ["linux"]
register = "template"
```

## Tiers

- `documented-only`: written from the tool's documentation; never executed by thegn. Recipes for services
  that need an account nobody holds yet ship at this tier.
- `fixture-tested`: its command lines are checked against recorded fixtures in unit tests.
- `live-tested`: run against the real tool at least once; `live_tested_on` records where and when.

A recipe never claims a higher tier than its evidence. Docs and doctor output may show the tier.

## Register

- `template` (the default): the recipe is only ever copied into the user's config by `recipe add`. It is not
  active until the user adds it.
- `shipped`: legal only for `kind = "sandbox"`. The definition becomes a shipped, named backend that thegn
  selects by name without the user copying it.

## Owners

| Kind                                                                                           | Owner                                                                           |
| ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `sandbox`                                                                                      | WS07 (runtimes and recipes): WS07-P4-01                                         |
| `provider`                                                                                     | WS08 (provider seam): WS08-P6-01                                                |
| `connector`                                                                                    | WS09 (connectors): WS09-P9-01, with WS05-P6-05's content for the Lima connector |
| Shared infrastructure (embed table, header validation, `INDEX.md` generator, config insertion) | WS08-P6-00                                                                      |
| Header type (`RecipeHeader`)                                                                   | WS00-P1-08, then WS08-P6-00                                                     |

WSL connector recipes are generated by WS06 (`thegn wsl connector`), not stored here.
