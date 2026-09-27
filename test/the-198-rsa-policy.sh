#!/usr/bin/env bash
set -euo pipefail

# Dependency-policy regression for THE-198/THE-670. This is intentionally
# dependency-free: it checks the committed TOML/lock evidence without Cargo.
script_dir=$(dirname -- "$0")
repo=$(cd -- "$script_dir/.." && pwd)
cd "$repo"

python3 - <<'PY'
import tomllib
from pathlib import Path

deny = tomllib.loads(Path("deny.toml").read_text())
ignored = deny["advisories"]["ignore"]
matches = [item for item in ignored if isinstance(item, dict) and item.get("id") == "RUSTSEC-2023-0071"]
assert not matches, "THE-670 must retire the rsa advisory exception"
assert "RUSTSEC-2023-0071" not in [item for item in ignored if isinstance(item, str)]

lock = tomllib.loads(Path("Cargo.lock").read_text())
names = {p["name"] for p in lock["package"]}
assert "octocrab" not in names
assert "jsonwebtoken" not in names
assert "rsa" not in names
print("THE-670 confirms octocrab, jsonwebtoken and rsa are absent from the lockfile")
PY
