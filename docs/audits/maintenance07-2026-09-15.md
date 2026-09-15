# Maintenance 07 verification — 2026-09-15

THE-222, THE-247, and THE-326 share one reviewed candidate and one native test
compilation. The tested source is `72d161b2e1868561fd2f5bce9facde8b0345512a`,
based on local main `f293594d4a8516f940540b4eda60a9a98ac1d7a1`.

## Changes and acceptance

- **THE-222:** Cross-host Git bundles use an unpredictable private directory,
  an exclusively created regular leaf, and a retained file handle through
  fetch. Creation refuses existing files, symlinks, hardlinks, directories,
  and FIFOs. Cleanup checks observed identity and never recursively removes a
  replacement. The Linux fixtures cover hostile entries, two owned competing
  child processes, unchanged canaries, mode checks, write/flush failures,
  unwind, and real Git bundle success/failure cleanup.
- **THE-247:** Named-volume sources use the documented ASCII engine-compatible
  grammar. Core entry and OCI argv construction refuse invalid sources;
  agent and terminal launch paths check configuration before resolver/fallback
  and check the final composed spec before VPN, ensure, secrets, and launch
  effects. Typed bounded diagnostics report the pair index without echoing
  the source. Docker/Podman planning, path variants, explicit host behavior,
  and production pre-effect callback sentinels are covered.
- **THE-326:** FRP configuration uses typed serde TOML serialization followed
  by strict typed reparsing and equality verification. Addresses, DNS labels,
  proxy names and applicable ports are validated before secret resolution.
  Raw extra lines are refused. Tests cover UTF-8 tokens, quotes, backslashes,
  controls, duplicate keys, injected tables, IPv6, all supported proxy wire
  shapes, and redacted file-plan debug output.

## Verification

Strict workspace Clippy with all targets and `-D warnings` passed at the exact
tested source. The shared core, svc, and host test compilation passed. Each of
145 selected tests ran in a fresh isolated process with one test thread and a
30-second outer timeout: **77 core, 16 svc, 52 host; all passed**. Native
execution took approximately 18 seconds in total. Both compilation and the
native matrix used a verified `cpu.max` of `100000 100000`; compilation used
one Cargo job and niceness 10. No release build was required.

An independently reviewed additional harness exercised the already compiled
Rust exclusive-create helper with two simultaneous attempts against each of
five preinstalled hostile entries: symlink, regular file, hardlink, FIFO, and
directory. All ten children returned the expected refusal; entry identity and
byte canaries were unchanged, and every child settled before exact nonrecursive
cleanup. All five cases passed with no additional compilation. This fills the
literal two-process hostile-entry acceptance gap identified by final review.

The manifest records source, binary, raw-log, selection and review hashes.
Cargo's `fresh=false` artifact field means these harnesses were newly compiled.
Two earlier strict-lint failures and a rejected `cargo test --keep-going`
invocation are retained. That rejected invocation did not compile; the
corrected invocation compiled the three harnesses once. Historical source-only
reports describe earlier checkpoints and do not override the final receipts.

## Limits and residual work

This is Linux native evidence, with no Windows runtime, live FRP/OCI provider,
release performance, coverage-percentage, or full-workspace native test claim.
The Windows exclusive-create, protected owner-SID DACL and stable file-identity
path was source-reviewed; native Windows proof remains unavailable.
Identity revalidation detects observed replacement and is not an atomic lease
against arbitrary same-user mutation. On uncertain/replaced identity, cleanup
preserves the entry rather than removing an object it cannot prove it owns.

The sibling temporary-path audit found `tg-drivermerge` and `tg-foldregen`
worktree ownership belongs to THE-394; sandbox secret files belong to THE-224;
remote bundle transport/streaming belongs to THE-223 and related remote-sync
work. THE-328 owns share-file materialization, and THE-325 owns share-client
credential transport. These issues remain separate and are not claimed fixed
by the three changes here. The original inventory is preserved in the artifacts.

Raw evidence lives in [the manifest](maintenance07-2026-09-15/manifest.json).
Root and independent source/evidence reviews accept the three scoped fixes.
The initial THE-222 acceptance qualification is superseded by the independently
reviewed five-case native supplement. Local-main landing follows final delivery
gates; Linear completion is recorded after the actual landing. The tested-source
hash does not claim any running live process has been rebuilt or restarted.
