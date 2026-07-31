# Ship thegn in a stable and a dev release channel

## Summary

thegn is approaching a pre-alpha release, but several subsystems are still
experimental: remote worktrees ([sandbox.remote], native russh is a stub),
cloud providers, the LLM proxy + agents ([llm_proxy], [[agents]]), the Observe
dashboards ([observe]), the placement engine ([placement]), and the non-GitHub
issue trackers ([issues]: Linear / Jira / Kaneo). Today every one of these is
compiled unconditionally and gated only by its own scattered `enabled` toggle,
so a user of a "stable" build can flip an experimental key and land in
half-finished territory, and there is no single place that says what is
shippable.

This change introduces a **release channel** — `stable` (the regular pre-alpha)
or `dev` — as a first-class, one-source-of-truth concept, without compiling any
code out (which would fight the "additive / always-fallback" architecture):

1. **Capability registry** (`thegn_core::channel`). A pure, unit-tested map of
   each gated `Feature` to a `Stability` (`Stable` / `Experimental`) plus
   `Feature::allowed_in(channel)`. Graduating a subsystem is a one-line edit.
2. **Config clamp** (`Config::clamp_to_channel`). At config load, the stable
   channel forces every disallowed feature's master toggle off — `llm_proxy`,
   `observe`, `placement`, `sandbox.remote.host`, provider `[host.*]`, and the
   non-GitHub `[issues]` providers/accounts (GitHub PR/issue viewing stays
   available). AI is gated at the proxy switch only; the `[[agents]]` launcher
   list (which holds the plain-shell entry) is untouched.
3. **Runtime holder + resolution** (host `channel_state`). The channel is
   resolved once at startup from `THEGN_CHANNEL` (env) → the `dev` Cargo feature
   default, installed into a lock-free atomic (same pattern as `caps`).
4. **Surfacing + guards.** `thegn doctor` prints the resolved channel and a
   per-feature allow table; experimental CLI verbs (`proxy`, `agent`, `host`,
   `placement`, `kaneo`) are refused in the stable build with a pointer to the
   dev channel; the compositor shows a one-line status note when a clamp fired;
   config-gated UI (e.g. the Observe app tab) disappears for free because the
   clamp turned its toggle off.
5. **Packaging.** A `dev` Cargo feature on `thegn-host` flips the compiled-in
   default channel (empty feature — no extra code). Nix exposes `.#dev`
   (`thegn-dev` / `tg-dev`, coexists with a stable install); `.#default` stays
   stable. `just ci` keeps the dev channel compiling; `just start-dev` runs it.

## Impact

- **tasks.md**: cross-cuts the release-readiness of groups covering remotes
  (L2 remote), providers, proxy/agents (U/V/W, Q–T), observe (S), placement,
  and trackers (AT) — this is the gate that lets the AI-free shell ship first.
- **thegn-core** — new `channel` module + `Config::clamp_to_channel` (+ tests;
  keeps the 95% core coverage gate green).
- **thegn-host** — new `channel_state` module; startup wiring in `run.rs`;
  CLI resolve/clamp/guard in `main.rs`; `doctor` channel report; `dev` feature.
- **nix / justfile / config.toml.example** — `.#dev` output, dev-channel CI
  check, `start-dev`, and a documented channel note.
- No new runtime dependencies; nothing is compiled out; fully reversible.
