# Add a coding-agent harness

A "harness" is a coding-agent CLI (Claude Code, Codex, aider, …). Every
per-vendor fact about one lives behind the **one** object-safe seam
`thegn_core::harness::Harness` (`crates/thegn-core/src/harness.rs`), so adding a
harness is a single implementation, not a sweep across the account table, the
headless-launch match, the usage parsers, the session walkers, and the sandbox
login-carry. The registry is **closed**: an id outside it is refused, never a
guessed command.

1. **Impl**: add a zero-sized `struct Gemini;` and `impl Harness for Gemini`
   in `crates/thegn-core/src/harness.rs`. Required ops:
   - `id` / `display_name` / `interactive_command` (`"gemini"`),
   - `login_argv` (the interactive-login argv, `&[]` if the harness authenticates
     via env keys),
   - `home` → `Some(HomeSpec { home_env, default_dir, auth_marker, auth_files })`
     when the credentials live in a relocatable home (this is what makes it
     account-switchable and drives the sandbox login-carve); `None` otherwise,
   - `headless_template` → the `{prompt}`-placeholder command, or `None`,
   - `model_flag` → the `{model}`-placeholder switch (`"--model {model}"`,
     `"-m {model}"`) that `[[agents]].model` / a stage `model` renders through;
     required for any harness with a headless form
     (`model_flags_are_model_templates`), `None` only for a harness with no
     model switch — a configured `model` then fails `config validate`,
   - `caps` → the `HarnessCaps` bits you implement (see below).

   `Pi` is the smallest complete example: interactive `pi`, headless
   `pi -p {prompt}`, model `--model {model}` (pi models are `provider/id`),
   no credential home (its home moves via `PI_CODING_AGENT_DIR` in an
   `[[agents]].env` overlay instead), `HarnessCaps::NONE`.

2. **Register**: add `static GEMINI: Gemini = Gemini;` and `&GEMINI` to the
   `HARNESSES` array. Order matters only for the `account::providers()`
   projection (it keeps `[codex, claude]`); append new harnesses after those.
3. **Optional ops — present iff the cap bit is set** (`caps_agree_with_ops`
   pins this):
   - `SESSIONS` → `session_layout` (store subdir + filename shape) **and**
     `parse_session_summary` (a credential-free one-line summary + recorded cwd).
   - `RESUME` → `resume_command(id)` returning the resume command with the id
     shell-quoted. Ids are validated by `session_id_ok` before you see them.
   - `USAGE` → `parse_usage(bytes, now)`, delegating to a pure parser in
     `thegn_core::usage` (keep the byte-level parser there, where its fixture
     tests live — the seam consolidates the dispatch, not the parsing).
   - `TOKENS` → `fold_transcript(...)` for the host-wide token rollup
     (Claude-shaped transcripts only).
   - `TEAMMATES` is **reserved** — do not advertise it until a follow-up change
     specs the on-disk shape against a real teammate session.
4. **Config cannot define a harness.** That would be arbitrary-command execution
   from config, and parsers can't be expressed in TOML. The closed registry is
   the door; harness-adapter **plugins** (roadmap P 200) are the future one.

Everything above is **pure** and unit-tested (the 95% core coverage gate). The
filesystem walk, the opt-in live usage fetch, the spawn, and the doctor probe
are driven by `thegn-svc` / `thegn-host` from the seam's data:

- **Launch / resume**: `thegn-host/src/daemon/agent_open.rs` resolves the command
  through the seam; `AgentLaunch.resume` carries an explicit session id.
- **Session discovery**: the generic walker in `thegn-svc/src/sessions.rs` drives
  every `SessionLayout`; it feeds `agent.sessions` (CLI `thegn agent sessions
--json`, MCP `agent_sessions`, HTTP `/v1/agent/sessions`).
- **Usage**: `thegn-svc/src/usage.rs` routes the three parse sites through the
  seam.
- **Account switching / sandbox carve**: `thegn_core::account::providers()` is a
  projection of the relocatable-home harnesses; `bundle.rs` / `sandbox_mounts.rs`
  read it unchanged.
- **Doctor**: `thegn doctor` prints a probe row per harness (binary on PATH,
  credential home, login state, session store) — add nothing; it iterates
  `HARNESSES`.

**Gates:** `caps_agree_with_ops`, `every_resume_impl_quotes_its_id`,
`session_layout_matches_its_own_fixture`, the per-impl usage/summary unit tests
(all in `harness.rs`); `providers_projection_matches_the_pre_seam_table`
(`account.rs`); and, for the `agent.sessions` capability, the catalog coverage
tests in §`capability.md`.
