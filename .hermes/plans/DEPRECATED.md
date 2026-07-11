# DEPRECATED — superseded by OpenSpec

This directory held ad-hoc, agent-executable task breakdowns from the old
"subagent-driven-development" flow. As of the OpenSpec adoption, thegn's
development is managed with **OpenSpec** instead.

**Do not add new plans here.** Start new work with the OpenSpec workflow:

```sh
just openspec-setup        # one-time per checkout: regenerate /opsx commands
# then in Claude Code:
/opsx:explore  "rough idea"   # think it through (no code)
/opsx:propose  "change-name"  # generate proposal + design + tasks + delta specs
/opsx:apply                   # implement against the agreed spec
/opsx:sync                    # merge delta specs into openspec/specs/
/opsx:archive                 # archive the completed change
```

- Specs (source of truth): `openspec/specs/`
- In-flight changes: `openspec/changes/`
- Roadmap index: `tasks.md`
- Full guide: the "Spec-driven development (OpenSpec)" section in `CLAUDE.md`

The existing files in this directory are retained for historical reference only.
