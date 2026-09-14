# Design

`argv` is mutually exclusive with `command`; each array entry is one argument, with a literal executable. Shell templates containing placeholders require `template_policy = "safe"`. Whole unquoted argument placeholders are quoted once. Shell assignments, redirections, control structures, substitutions, comments, functions and embedded/quoted placeholders are conservatively refused. Static scripts without placeholders retain previous shell semantics.

`dangerously_allow_raw` plus `dangerously_unquoted_shell` is the explicit unsafe escape hatch. Neither implicit legacy raw nor unknown filters are admitted. Typed `ExpandedCommand` retains provenance through popup/none/terminal dispatch; Debug and error paths omit data. Terminal argv launches without shell reinterpretation; shell templates explicitly use POSIX sh. SSH/provider projections for popup/none quote once at the transport edge. Terminal routing remains the existing local/daemon-backed path (THE-379), so fixtures execute remote projections locally and do not claim a real remote terminal session. A program itself may interpret its arguments (for example eval); trusted configuration remains executable authority.

Invalid command configuration produces field-indexed diagnostics and runtime refusal without discarding unrelated configuration. Strict config writes reject invalid command definitions. Template/argv/output bounds and NUL checks reject before execution. Field resolution borrows untrusted strings; cumulative raw and quoted-size budgets are checked before append or quote allocation. Config warnings use the existing deduplicated diagnostic path.

No arbitrary OS process cancellation or remote ownership guarantee is claimed here; THE-378 remains the command lifecycle issue.
