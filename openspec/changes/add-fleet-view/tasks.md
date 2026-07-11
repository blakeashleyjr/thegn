# Tasks

## 1. Authoritative metrics (thegn-proxy / thegn-core)

- [ ] 1.1 Extend `parse_usage` to also extract `cache_creation_input_tokens` /
      `cache_read_input_tokens` into `ProxyRequestRow` — **unit tests**: full split
      parsed, missing cache fields default to zero, malformed usage does not panic.
- [ ] 1.2 A read-time per-worktree aggregation over `proxy_requests` (turns, token
      totals + split, token-rate series) — **unit tests**: rollup sums by worktree
      over a window, empty history yields zeros.

## 2. Compaction detection (thegn-core)

- [ ] 2.1 Pure `detect_compaction(history, threshold) -> Vec<usize>` flagging
      turns whose context dropped over threshold — **unit tests**: drop-over
      threshold flags, small dip does not, first turn never flags, monotone growth
      never flags.

## 3. Fleet model + surfaces (thegn-host)

- [ ] 3.1 Add `per_worktree_metrics: HashMap<String, FleetMetrics>` to `FrameModel`
      and a `RefreshKind::Fleet` off-loop hydrator (channel + `TerminalWaker`).
- [ ] 3.2 Fleet panel/overlay: per-worktree rows (context %, token split,
      sparkline, turns, compactions, current task, children+ports) + a live
      tool-call timeline for the focused agent — **render test**: the live strip is
      an `Incremental { bars }` / `Panes` bounded diff, never a Full recompose.
- [ ] 3.3 Orphan-port flag: mark a detected `forward.rs` port whose owning process
      has exited.
- [ ] 3.4 `thegn fleet [--json]` subcommand emitting the read-only rollup (the
      `cmd/issue.rs`/`config.rs` `--json` pattern); it MUST NOT issue a model call.

## 4. Docs + validate

- [ ] 4.1 Document the fleet view, its metrics, and `thegn fleet --json` in the
      agent/perf doc section + `config/config.toml.example`.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
