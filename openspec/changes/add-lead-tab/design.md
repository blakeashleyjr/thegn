# Design — the Lead tab

The Lead reuses the terminal registry and `workspace_pool::take_terminal_group`,
so it is global across workspaces with no schema change. The launch path builds
a bwrap argv with the notes dir writable, repos read-only, and the daemon socket
and run directories masked. Its skill is seeded into
`<notes_dir>/.claude/skills/lead/SKILL.md`, rewritten only when content differs.
With `[lead]` absent nothing runs; with it present only a blocking loopback
accept exists until the Lead is opened.

Render: the Lead row changes sidebar rows, so a Full recompose on state change; pane output stays Panes. Help: docs/help/lead.md, context zone:sidebar. Schema: none.
