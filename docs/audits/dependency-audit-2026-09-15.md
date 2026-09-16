# Dependency audit — 2026-09-15

Scope: the resolved dependency graph of the shipped `thegn` binary. Every number
below is measured on this checkout at `2688c68c`, not estimated. Method notes are
at the end so each figure can be re-derived.

## Status

Applied the same day, in this branch:

| #   | Action                                       | Result                                                                                                                                                                                              |
| --- | -------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `cargo update -p rustls` (0.23.43 → 0.23.45) | RUSTSEC-2026-0285 cleared; `cargo deny check advisories` green                                                                                                                                      |
| —   | `cargo update -p chacha20` (0.10.1 → 0.10.2) | yanked-crate warning cleared                                                                                                                                                                        |
| 2   | `tokei` 12 → 15                              | **zero code changes needed**; `clap 2.34`, `atty`, `ansi_term`, `instant`, `vec_map`, `num_cpus`, `textwrap` removed; `strsim` and `parking_lot` de-duplicated; **3 of 8 advisory ignores retired** |
| 4   | Removed orphaned `polars-*` artifacts        | 279 MB; confirmed absent from `Cargo.lock` and from every manifest and test target                                                                                                                  |
| 3   | Replace `serde_yaml`                         | **Deferred — see the correction below.**                                                                                                                                                            |

Corrections to this report, found while applying it:

- **The `serde_yaml` recommendation was wrong as written.** It cited
  `serde_norway`, `serde_yaml_ng` and `saphyr` as "live replacements". Measured:
  `serde_yaml_ng`'s last publish is 2024-05-26 (two months _after_ the crate it
  replaces), `serde_norway`'s is 2024-12-21, and `serde_yml` is a `0.0.x` line.
  Meanwhile `serde_yaml` still takes 89M downloads per 90 days and carries no
  advisory. Swapping a deprecated-but-stable pure-Rust parser for a less-used
  fork that is itself stale is not an improvement. The real exposure is a future
  CVE never being fixed, and the parser only ever sees local config files and
  tmuxinator layouts here — not untrusted network input. **Revisit if an advisory
  lands or if a successor consolidates the ecosystem.**
- **`term_size` stays.** tokei 15 still depends on it directly, so
  RUSTSEC-2020-0163 remains ignored. Four ignores were attributed to tokei; three
  were retired, not four.
- **The "orphaned artifacts" finding was narrower than the 200 MiB figure
  suggested, and a larger one sits behind it.** `polars` was genuinely foreign
  (279 MB, removed). But a scan for "artifacts with no `Cargo.lock` entry" mostly
  returns _integration-test binaries_ (`tests/*.rs` targets have no lockfile
  entry by design) — those are legitimate. What they do reveal is **42 retained
  generations of each test binary**: `iroh_fly_live` alone holds 1.53 GiB across
  42 files, and 15.1 GiB sits in such stacks. `target/` is **91 GiB**. 5.75 GiB
  of `deps/` is older than 30 days. Cargo never reaps old generations and
  `cargo-sweep` is not installed; note that an old mtime does **not** prove an
  artifact is dead, since cargo does not touch files on a cache hit, so an
  age-based prune trades disk for rebuild time rather than being free.

## Shape

| Measure                                              | Value                  |
| ---------------------------------------------------- | ---------------------- |
| Direct workspace dependencies                        | 55                     |
| External crates in `thegn-host`'s normal-dep closure | 886                    |
| Package entries in `Cargo.lock`                      | 953 (840 unique names) |
| Crate names resolved at 2+ versions                  | 86                     |
| Native (`-sys`/C) dependencies                       | 27                     |
| Release binary, stripped, `lto = "thin"`             | 118 MB                 |

Two things are already right and should be said before the criticism: `cargo
machete` reports **no unused dependencies**, and `deny.toml`'s advisory `ignore`
list gives every entry a reason and an exit condition. This is a well-kept graph.
The problem is not hygiene. It is that a handful of dependencies cost far more
than the use being made of them.

## The core finding: cost and benefit are badly misaligned

"Crates removed" is what disappears from the closure if that dependency is
blocked — computed on the real resolved graph, so shared subtrees are not
double-counted. "Refs" and "files" are in-tree call sites in `crates/*/src`.

| Dependency   | Crates removed | Refs | Files | Ratio                        |
| ------------ | -------------: | ---: | ----: | ---------------------------- |
| `iroh`       |    119 (13.4%) |   16 |     2 | **7.4 crates per reference** |
| `gix`        |      75 (8.5%) |   32 |     4 | 2.3                          |
| `fff-search` |      35 (4.0%) |    6 |     1 | **5.8**                      |
| `zbus`       |      33 (3.7%) |    7 |     1 | **4.7**                      |
| `octocrab`   |      31 (3.5%) |   17 |     3 | 1.8                          |
| `termwiz`    |      30 (3.4%) |  307 |   144 | 0.1 — earns its place        |
| `tokei`      |      26 (2.9%) |    3 |     2 | **8.7**                      |
| `image`      |              8 |   32 |    12 | 0.25                         |
| `syntect`    |              4 |    4 |     1 | 1.0                          |
| `axum`       |              4 |  182 |    22 | 0.02                         |
| `tonic`      |              2 |    8 |     3 | 0.25                         |

Five dependencies — `iroh`, `gix`, `fff-search`, `zbus`, `tokei` — account for
**288 crates, 32.5% of the entire closure**, against 64 call sites in 10 files.

Everything is unconditional. Only `tonic`/`prost` (`control-grpc`) and `pprof`
(`profiling`) are optional deps anywhere in the workspace — and `thegn-host`
enables `control-grpc` in its own manifest, so the "additive for external
tooling" comment on that feature is not true of the shipped binary.

## Quality and popularity

Downloads from the crates.io API, 2026-09-15. "Stale" = days since the crate was
last published.

### Load-bearing and healthy — leave alone

| Crate      | 90-day downloads | Latest  | Stale |
| ---------- | ---------------: | ------- | ----: |
| `libc`     |             358M | 0.2.189 |   43d |
| `serde`    |             306M | 1.0.229 |   59d |
| `regex`    |             246M | 1.13.1  |   62d |
| `clap`     |             229M | 4.6.7   |    1d |
| `tokio`    |             223M | 1.53.1  |   57d |
| `reqwest`  |             182M | 0.13.5  |    7d |
| `axum`     |             115M | 0.8.9   |  154d |
| `rusqlite` |              34M | 0.40.2  |   38d |

Top-tier by every measure. `tokio`'s features are enumerated rather than `full`,
and 10 of 23 table-specced deps set `default-features = false` — good hygiene.

### Good crates, disproportionate for the use

- **`gix`** (10M/90d, 0.87.1, 22d). Genuinely high quality and among the most
  actively developed projects in the ecosystem. It is also 75 crates, and it is
  reached from exactly one file, `thegn-svc/src/git/native_diff.rs`, behind a CLI
  fallback that already exists and already works. The module doc says the native
  impl "lands in Phase 2" — we are paying Phase 2's bill in Phase 1.
- **`iroh`** (1.6M/90d, 1.2.0, 6d). Real project, actively developed, but young:
  2.5M downloads all-time against 1.6M in the last 90 days means essentially all
  adoption is recent. It drags a parallel async ecosystem (`n0-future`, `moka`,
  `loom`), three `hickory-*` DNS crates, the NAT-traversal stack
  (`igd-next`, `portmapper`, four `netlink-*`), **JNI for Android**, and five
  `objc2-*` Apple frameworks — into a terminal multiplexer, for one file.
- **`octocrab`** (2.2M/90d, 0.54.2, 1d). The standard GitHub crate and fine, but
  the 90-day/all-time ratio shows a modest user base, and it brings the entire
  previous-generation RustCrypto stack (`rsa`, `p256`, `p384`, `ecdsa`,
  `jsonwebtoken`, `num-bigint-dig`) for GitHub App JWT signing. A `gh` CLI forge
  path already exists alongside it.
- **`zbus`** (24M/90d, 5.19.0, 37d). Healthy. But it is in the tree **twice** —
  v4.4.0 via `secret-service` ← `keyring`, and v5.19.0 directly — so both
  `zvariant` generations, both macro crates, and two `async-*` executor stacks
  compile. Seven references in one file (MPRIS media control).

### Weak links

- **`fff-search` = `"=0.10.6-nightly.611dd87"`**. 175k downloads all-time, of
  which 132k are from the last 90 days: this crate is weeks old in practice.
  Pinned to an exact nightly prerelease because, as the manifest comment says,
  "fff publishes only prereleases (no stable line yet)." It is the single
  largest supply-chain risk in the graph, and it drags **`libgit2-sys`,
  `lmdb-master-sys`, and `libz-sys`** — three vendored C libraries — for one file
  of file-picker code. Its sibling `neo_frizbee` is pinned at `0.10` while
  `0.13.1` shipped today.
- **`serde_yaml` = `"0.9"`**. Resolves to `0.9.34+deprecated`, last published
  **2024-03-25 (905 days ago)**. The author formally deprecated it. Used in
  `thegn-core`, 14 refs across 4 files. Live replacements exist (`serde_norway`,
  `serde_yaml_ng`, `saphyr`).
- **`tokei` = `"12"`**. The `deny.toml` comment says "tokei 12.1.2 is the latest
  release" — **that is stale: tokei 15.0.0 shipped 2026-09-06.** The major-version
  pin is what keeps `clap 2.34`, `atty`, `ansi_term`, `term_size`, `instant` and
  `parking_lot 0.11` in the tree, and what keeps four advisory ignores alive. All
  of this for three references.

### Vendored

- **`termwiz`** at `vendor/termwiz` (10M/90d, 0.23.3 — but **544 days stale**).
  307 refs across 144 files: the most load-bearing dependency we have, and the
  most justified. The vendoring (PATCH.md, THE-618) is sound. The risk is that
  upstream has not published in 18 months, so we should assume we now own it.
  `portable-pty`, from the same project, is 581 days stale.

## Security

`cargo deny check advisories` **fails** on one live vulnerability:

- **RUSTSEC-2026-0285** — `rustls 0.23.43` accepts TLS 1.3 handshake messages at
  the wrong encryption level. A single version is in the tree, reached by both
  `reqwest`s, `iroh`, `octocrab`, `tonic`, `axum`, `tokio-tungstenite` and
  `hickory`. Not remotely exploitable to alter a handshake, but it is a real
  advisory and the fix is a lockfile bump.

Licences pass. Bans pass. Of the eight documented advisory ignores, **seven are
purchased by the low-value dependencies above**: four by `tokei`, one by
`octocrab` → `jsonwebtoken` → `rsa` (RUSTSEC-2023-0071, the Marvin timing
sidechannel — the only _vulnerability_ among the ignores), one by `syntect`, one
by `iroh` → `netlink-packet-core` → `paste`.

## What is holding us back

**1. Cross-compilation, and therefore the Windows port.** `just check-cross`
cannot check `thegn-core`, `thegn-svc`, `thegn-host` or `gtui-query` without a
real cross C toolchain, and names the reason: "ring, bundled sqlite and libgit2".
Of those three, `libgit2` — plus LMDB and zlib, which the justfile does not
mention — arrive **solely through `fff-search`**, a pinned nightly prerelease
used in one file. `ring` arrives through `iroh` _and_ `rustls`, so TLS keeps it
regardless. Bundled SQLite is a deliberate choice worth keeping.

**2. Duplicate-version compile tax.** `windows-sys` resolves at **six** versions
(0.45, 0.48, 0.52, 0.59, 0.60.2, 0.61.2), `windows-targets` at four, and each of
the eight per-architecture import-library crates at four. Those crates ship large
binary import libraries. `syn` is present at three versions (1.0.109, 2.0.119,
3.0.3) — the heaviest proc-macro dependency in the ecosystem, compiled three
times. `nix` at four, `rand`/`rand_core`/`getrandom` at three each, `reqwest` at
two (0.12 and 0.13 — two complete HTTP stacks), `zbus` at two majors. The whole
RustCrypto generation boundary is duplicated: `digest` 0.10/0.11, `sha2`
0.10/0.11, `ed25519` 2/3, `curve25519-dalek` 4/5, `der`, `spki`, `pkcs8`,
`signature`, `const-oid`.

**3. Three git implementations ship in one binary.** The `git` CLI (the declared
source of truth), `gix` 0.84 (for one diff module), and `libgit2` 0.18.8 via
`git2` 0.21 (transitively, through `fff-search`). Two embedded databases as well:
SQLite via `rusqlite` and LMDB via `heed`/`lmdb-master-sys`.

**4. Binary size.** 118 MB stripped. For comparison, `ripgrep` is ~5 MB and
`helix` ~30 MB. This is the direct consequence of an 886-crate closure linked
into one artifact.

**5. Version drift on the exact crates that hurt.** `tokei` 12 → 15, `gix` 0.84 →
0.87.1, `tonic` 0.13 → 0.14.6, `tree-sitter` 0.26 → 0.27, `chrono-tz` 0.9 →
0.10.4, `neo_frizbee` 0.10 → 0.13.1, `iroh` lock at 1.0.3 → 1.2.0. The stale
`deny.toml` comment about tokei shows how this compounds: a pin outlives the
reason for it, and the justification calcifies into documentation.

**6. `target/` carries dead weight.** `target/release/deps` holds ~200 MiB of
`polars-*` rlibs. **`polars` is not in `Cargo.lock`** — these are artifacts of a
build that no longer exists, never reaped. `thegn_core` has 19 distinct rlib
hashes retained, up to 121 MiB each.

## Recommendations

Ordered by payoff against effort. Nothing here is a behaviour change to the
product except where stated.

### Do now — hours, no design work

1. **`cargo update -p rustls`.** Clears the one failing advisory.
2. **Upgrade `tokei` 12 → 15.** Very likely retires `clap 2`, `atty`,
   `ansi_term`, `term_size`, `instant` and four advisory ignores in one commit.
   Verify against the three call sites and delete the stale `deny.toml` comment
   either way.
3. **Replace `serde_yaml`.** It has been deprecated for two and a half years;
   14 refs in 4 files is an afternoon.
4. **`just clean-aux` and reap the `polars` artifacts.** ~200 MiB back for free.

### Do next — the structural wins

5. **Feature-gate `iroh` behind a non-default cargo feature**, exactly as
   `control-grpc` is done. −119 crates (13.4%) from the default build, for a
   capability reached by one file. This is the single highest-value change in the
   audit and it removes nothing from anyone who opts in.
6. **Take the file picker in-house and drop `fff-search`.** −35 crates, and it
   removes `libgit2`, `LMDB` and `libz` — the avoidable half of the
   cross-compilation blockage, which is the thing standing between us and a
   checkable Windows build. The pieces are already present: `ignore` for the
   walk, `neo_frizbee` (already a _direct_ dependency) for SIMD matching, and
   SQLite for frecency instead of a second embedded KV store. Retires a pinned
   nightly prerelease from the critical path.
7. **Feature-gate `octocrab`.** −31 crates and the `rsa` Marvin ignore. The `gh`
   CLI forge path already exists, which is what makes this cheap.
8. **Collapse the `zbus` duplication.** Align `secret-service`/`keyring` onto
   zbus 5, or gate media control. −33 crates for the full removal; the dedup
   alone is worth it.

### Measure before deciding

9. **`gix` (−75 crates) needs a number, not an opinion.** It exists to make the
   hot panel-poll diff fast, and a CLI fallback already covers the same surface.
   Benchmark `native_diff::diff_entries` against the CLI path on a large
   worktree. If the native path wins materially, 75 crates is a fair price and
   the module doc should say so with the measurement attached. If it does not,
   this is the second-largest removal available.

### Explicitly leave alone

`termwiz` (307 refs — the product), `axum` (182 refs, 4 exclusive crates),
`tokio`, `serde`, `clap`, `regex`, `rusqlite` with bundled SQLite, `image` (real
sixel/kitty rendering), `tree-sitter` and its five grammars (genuine semantic
parsing in `semantic.rs`), `syntect` (4 exclusive crates — cheap). `criterion`
removes **zero** crates from the binary closure; it is dev-only and costs
nothing at runtime.

### The honest summary

Recommendations 1–8, taken together, remove roughly **220 of 886 crates (25%)**
from the default build, retire six of eight advisory ignores plus the live
`rustls` vulnerability, and delete two of the three vendored C libraries blocking
`check-cross`. None of it requires a product decision — every capability either
survives behind a feature flag or already has a working second implementation in
the tree.

## Method

- Closure and exclusivity: `cargo metadata --all-features`, normal edges only
  (dev and build dependency kinds dropped), reachability from `thegn-host` with
  each candidate blocked graph-wide. Script retained in the session scratchpad.
- Duplicates: parsed from `Cargo.lock` package entries.
- Call sites: regex `(?<![A-Za-z0-9_])<crate>::` over 1108 files in `crates/*/src`.
  Undercounts crates whose items are re-exported, so treat refs as a floor —
  `tree-sitter` in particular reads as 1 ref but is used through its five grammar
  crates.
- Popularity and staleness: crates.io API, 2026-09-15.
- Advisories/licences/bans: `cargo deny check`, advisory-db as of this run.
- Binary size: `target/release/thegn`, `file(1)` confirms stripped.
