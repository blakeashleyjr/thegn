# Primary review + greenlight — THE-676

Reviewing row 518's investigation (`.thegn/pipeline/THE-676/maintenance-investigate/518.md`).

**Verdict: APPROVED to implement, with three binding amendments below.**

The stale-inventory finding is accepted and is the most valuable part of the
investigation: `activity.rs` and `devcontainer_features.rs` are test-only on this
branch, so the issue's "ten production sites" is wrong and the real production
conversion surface is two sites. The per-site retain/convert triage matches the
coordination brief's rules (remote seam, configured command text, and the
`sha256sum | awk` pipeline all correctly retained).

Implement the plan as written **except** for these amendments.

## Amendment 1 (REQUIRED) — `which_path` is not `command -v`

The plan converts two `command -v` shells to `which_path`/`have`. The primary
verified the current implementation at `crates/thegn-core/src/util.rs:650-664`:

```rust
pub fn have(cmd: &str) -> bool { which_path(cmd).is_some() }
pub fn which_path(cmd: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;      // <- unset PATH => None
    for dir in std::env::split_paths(&paths) {
        let p = dir.join(cmd);
        if p.is_file() { return Some(...) }      // <- NO executable-bit check
    }
    None
}
```

Two semantic differences from `command -v` that the plan does not mention:

1. **No executable-bit check.** `is_file()` is true for a non-executable file.
   `command -v` only reports executables. So a conversion can report a tool
   present that the shell would not.
2. **Unset `PATH` yields `None`**, whereas `command -v /abs/path` succeeds
   without `PATH`. This matters for `agent.rs`, which probes `$SHELL` — that is
   frequently an **absolute path** (`/bin/zsh`). `dir.join("/bin/zsh")` returns
   `/bin/zsh` so it happens to work, but only when `PATH` is set at all.

You must do **both** of:

- Handle the absolute-path case explicitly in the `agent.rs` conversion: if the
  probed token is already absolute, check it directly rather than relying on the
  `join` coincidence.
- State the executable-bit difference in a code comment at the conversion sites
  and in your artifact. **Do not** "fix" `which_path` by adding an
  executable-bit check — it has ~30 callers and that is a separate behavioural
  change, not this mechanical lane. If you believe it is a genuine bug, report
  it as a follow-up finding instead of changing it.

## Amendment 2 (REQUIRED) — the cache must never panic

`have()` is called from launch/probing paths. A `Mutex` poisoned by a panic in
another thread must not take down the caller: recover with
`lock().unwrap_or_else(|e| e.into_inner())` (or use a lock that cannot poison).
A cache is best-effort infrastructure — on any lock problem, fall through to an
uncached `which_path` probe rather than propagating. Add
`// best-effort: <why>` per the repo's ignored-result convention if you swallow
anything.

## Amendment 3 (accepted, restating for the record)

Positive-only caching is correct and stays: a hit is cached for the session, a
miss is re-probed every time. The comment must say why (a pane can `nix develop`
or `cargo install` mid-session). Keep `which_path` itself uncached.

## Scope lock

Production files you may touch: `crates/thegn-core/src/util.rs`,
`crates/thegn-host/src/cmd/doctor.rs`, `crates/thegn-host/src/agent.rs`.
Nothing else. No config key, no schema, no ratchet allowlist edit. If you find
you need one, STOP and report a blocker.

## Tests

The three cases in the plan (positive cached, miss re-probed then found,
concurrent) via an injected probe so no test mutates the process-global `PATH` —
approved as specified. `thegn-core` is gated at 95% lines, so every new branch
needs coverage. Add one test pinning the absolute-path probe from Amendment 1.

## Validation

Do not run cargo/nextest/clippy/lint/smoke. The primary runs the batch gate.
Record the exact commands needed; the order proposed in the investigation is
accepted.

---

## Amendment 2 — CLARIFIED by the primary after adversarial review (row 524)

Row 524 read Amendment 2 literally and filed a blocking finding: the cache
recovers a poisoned lock with `into_inner` but still trusts a cached positive
entry, rather than "falling through to an uncached probe".

**The primary overrules that finding. The implementation is correct; Amendment 2
was imprecisely worded.**

Reason: a poison flag is **sticky**. `PoisonError::into_inner()` returns the
data but does not clear the flag, so every subsequent `lock()` also returns
`Err`. Under the literal reading, a single unrelated panic anywhere that touched
this mutex would permanently revert `have()` to walking `PATH` on every call —
destroying the optimization this issue exists to add, and doing so invisibly.
The only work performed under the lock is `get`/`insert` on a
`HashMap<String, Option<String>>`; neither can leave a logically wrong value
behind, so the cached entries remain trustworthy after a panic elsewhere.

**The contract is therefore:** a poisoned lock must never propagate a panic and
must never disable the cache. Recover with `into_inner`, keep serving positive
entries, and keep re-probing misses.

The primary replaced row 524's failing test with
`a_poisoned_cache_keeps_serving_its_positive_entries`, which pins this contract
(including that the lock is genuinely poisoned, that a hit does not probe, and
that a miss still re-probes and fills).

Row 524's **second** finding is accepted and fixed: the concurrency test now
asserts a settled entry serves without probing.
