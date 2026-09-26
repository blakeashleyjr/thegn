# Primary plan review: revisions required before code

The investigation is accepted, but the migration outline is too broad for one implementation approval. Produce concrete ordered chunks with file scopes, API shapes, call-site inventory, failure semantics, and tests. No production edits or builds yet.

- Only a missing implicit default file may use first-run defaults. Explicit files and selected named profiles missing/unreadable/invalid are typed hard errors. Deprecation warnings remain warnings; schema/authority errors refuse publication. No silent fallback or clamping that relaxes policy.
- Preserve the documented untrusted repository-overlay boundary, but admit every effective authority configuration before use. A tolerant display projection cannot become authority. Do not expand into the separate default-sandbox issue.
- Admission precedes startup authority side effects. Keep disk work off the input/render loop. Failed reload retains last-good for display with a bounded coalesced diagnostic; new authority operations require a current admitted revision. Teardown must remain possible on config failure without newly granting authority.
- Config sources, selected profile, captured env/CLI values and strict DB-host composition form one coherent revision. Explain/get/health consume its normalized trace. Specify real stale-operation checks, not just an unused digest field.
- Propose explicit numeric budgets using real config sizes and existing checked-foundation limits: preparse file/line/depth and postparse collection/string/work bounds. Explain how multiline/quoted TOML avoids incorrect lexical depth checks.
- Inspect parked 99cec87a, 6c0f86d7, f3354d2c and relevant ancestors as source only. Identify useful primitives and unwired/unsafe parts. Do not merge or cherry-pick unrelated features. Primary review is required before reuse.
- Reject unknown Fly presets and malformed static provider specs before any remote requests. Do not add network preflight features. Bind provider specs to the admitted revision.

Deliver ordered serial chunks on this issue branch, a complete production consumer inventory (including standalone apps, completion, proxy and teardown), acceptance-to-test mapping, major risks, and a recommended first chunk. Keep main untouched until the complete issue is reviewed and tested. No builds or child agents.
