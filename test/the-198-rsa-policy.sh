#!/usr/bin/env bash
set -euo pipefail

# Dependency-policy regression for THE-198. This is intentionally
# dependency-free: it checks the committed TOML/lock evidence without invoking
# Cargo. The Rust service tests cover runtime forge behaviour separately.
script_dir=$(dirname -- "$0")
repo=$(cd -- "$script_dir/.." && pwd)
cd "$repo"

python3 - <<'PY'
import tomllib
from pathlib import Path

deny = tomllib.loads(Path("deny.toml").read_text())
ignored = deny["advisories"]["ignore"]
matches = [item for item in ignored if isinstance(item, dict) and item.get("id") == "RUSTSEC-2023-0071"]
assert len(matches) == 1, "THE-198 must have one structured advisory exception"
assert "RUSTSEC-2023-0071" not in [item for item in ignored if isinstance(item, str)]
reason = matches[0].get("reason", "")
for marker in ("rsa 0.9.10", "owner:", "review trigger:",
               "https://rustsec.org/advisories/RUSTSEC-2023-0071.html",
               "AppAuth", "private-key"):
    assert marker in reason, f"THE-198 exception is missing {marker!r}"

lock = tomllib.loads(Path("Cargo.lock").read_text())
packages = {(p["name"], p["version"]): p for p in lock["package"]}
octo = packages[("octocrab", "0.54.1")]
jwt = packages[("jsonwebtoken", "10.4.0")]
rsa = packages[("rsa", "0.9.10")]
assert "jsonwebtoken" in octo["dependencies"]
assert "rsa" in jwt["dependencies"]
assert rsa["version"] == "0.9.10"
print("THE-198 policy metadata and resolved octocrab -> jsonwebtoken -> rsa chain are pinned")
PY
