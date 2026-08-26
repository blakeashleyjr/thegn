# The exact source the package build depends on.
#
# This is an ALLOWLIST, and that is the whole point. It used to be a denylist
# (drop `target/`, `result/`, `.direnv/`, `.git/`), which meant every unrelated
# file fed the derivation hash: a CHANGELOG tweak, a workflow edit, an openspec
# change, a new muse spec. Each of those forced a full rebuild of ~600
# dependency crates plus `thegn` plus the musl bridge — the better part of an
# hour of CI for a docs commit, even with a warm store cache. Measured: touching
# `README.md` produced a different `.drv`.
#
# Everything cargo compiles, or the binary `include_str!`s, must be listed here.
# The non-obvious entries are load-bearing:
#   - `README.md`                     ← crates/thegn-host/src/cmd/mcp.rs
#   - `extensions/skills/mq/SKILL.md` ← crates/thegn-host/src/mq_assets.rs
#   - `config/**`                     ← thegn-core file_manager/yazi.rs + help/config_ref.rs
#   - `docs/help/**`, `docs/cli.md`   ← thegn-host help pages
#   - `docs/api/**`                   ← thegn-core tests/plugin_api_wire.rs snapshot
#
# If you add an `include_str!` that reaches outside `crates/`, add its path here
# or the sandboxed build fails with a missing file. Prefer whole directories
# over individual files so a new sibling doesn't silently break the build.
{lib}: let
  roots = [
    "Cargo.toml"
    "Cargo.lock"
    ".cargo"
    "crates"
    "config"
    "extensions"
    "docs/help"
    "docs/cli.md"
    "docs/api"
    "README.md"
  ];
  # Keep a path when it IS an allowed root, lives under one, or is a parent on
  # the way to one — nix has to descend `docs/` to reach `docs/help`.
  keep = rel:
    lib.any (
      r:
        rel
        == r
        || lib.hasPrefix (r + "/") rel
        || lib.hasPrefix (rel + "/") r
    )
    roots;
in
  root:
    lib.cleanSourceWith {
      src = root;
      name = "thegn-source";
      filter = path: _type: keep (lib.removePrefix (toString root + "/") (toString path));
    }
