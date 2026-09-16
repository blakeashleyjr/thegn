#!/usr/bin/env python3
import os, subprocess, sys, tempfile
from pathlib import Path
with tempfile.TemporaryDirectory(prefix="thegn-fullgate-state-") as tmp:
    keep = ("PATH", "LANG", "LC_ALL", "TERM", "CARGO_MANIFEST_DIR")
    env = {k:v for k,v in os.environ.items() if k in keep or k.startswith(("NEXTEST_", "CARGO_BIN_EXE_"))}
    for key, name in (("XDG_STATE_HOME","state"),("XDG_CONFIG_HOME","config"),("XDG_DATA_HOME","data"),("XDG_CACHE_HOME","cache"),("XDG_RUNTIME_DIR","runtime")):
        target=Path(tmp)/name
        target.mkdir(mode=0o700)
        env[key]=str(target)
    # Preserve inherited HOME unchanged only for reviewed path/planning tests.
    # Their bodies do not write to HOME; the agent-config case writes solely
    # to its own temporary directory. Directory-specific premises may still
    # be absent on this host and must not be fabricated by the runner.
    readonly_home_tests = {
        "sandbox::tests::cfg_mounts_covered_by_parent_are_skipped",
        "sandbox_mounts::tests::carveouts_are_rw_existing_paths_under_home",
        "sandbox_mounts::tests::keychain_carved_for_hardened_not_sealed",
        "sandbox_mounts::tests::nix_client_cache_carved_writable",
        "sandbox_mounts::tests::claude_profiles_carved_for_hardened_not_sealed",
        "sandbox_mounts::tests::claude_projects_carved_for_hardened_not_sealed",
        "sandbox_mounts::tests::history_files_carved_for_all_ro_profiles",
        "sandbox_mounts::tests::agent_config_dir_carved_for_hardened_not_sealed",
        "sandbox_mounts::tests::ro_home_flag_controls_home_writability",
        "resolve_wires_home_ro_from_profile",
        "usage::tests::configured_provider_drops_its_default_home_unconfigured_keeps_it",
        "build_cache::tests::sandbox_cache_mounts_always_covers_the_hook_frameworks",
    }
    if readonly_home_tests.intersection(sys.argv[1:]) and "HOME" in os.environ:
        env["HOME"] = os.environ["HOME"]
    env["GIT_CONFIG_GLOBAL"]="/dev/null"
    env["GIT_CONFIG_NOSYSTEM"]="1"
    result=subprocess.run(sys.argv[1:],env=env)
    sys.exit(result.returncode)
