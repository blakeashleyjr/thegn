### R. Agent integration protocols

_Reframed (2026-06-22): superzej's primary agent is the **embedded first-party
harness** `termite-agent` (the `agent` app tab). This group — ACP + native adapters
for **foreign** harnesses — is now an **additive, secondary** path, not the primary
one. "Primary path" below refers to ACP being the preferred way to integrate an
external harness, not to ACP being superzej's primary agent._

- [ ] 229. ACP client (primary path)
- [ ] 230. ACP session management
- [ ] 231. ACP streaming updates
- [ ] 232. ACP permission requests → UI
- [ ] 233. ACP diff rendering
- [ ] 234. ACP plan/tool-call events
- [ ] 235. ACP Registry integration (install agents)
- [ ] 236. Native adapter: Claude Code (hooks+stream-json+OTEL)
- [ ] 237. Native adapter: Codex (exec --json)
- [ ] 238. Native adapter: OpenCode (server API/SSE)
- [ ] 239. Native adapter: aider (scripting)
- [ ] 240. Top-10 harness support
- [ ] 241. Plugin adapters for the long tail
- [ ] 242. Per-harness capability detection + fallback
- [ ] 657. Agent hook passthrough — run the repo's existing `.claude/`/`.codex/` hooks when launching a harness, plus worktree setup/post-create hooks (deps install, env restore); surface `CLAUDE.md`/`AGENTS.md` in the file tree for inline editing, untouched (Orca; extends D 54, P 205, AR 547)
- [~] _(current: `pick_agent` launches claude/aider/shell as the worktree process)_
