# Design

`argv` is mutually exclusive with `command`; each array entry is one argument, with a literal executable. Shell templates containing placeholders require `template_policy = "safe"`. Whole unquoted argument placeholders are quoted once. Shell assignments, redirections, control structures, substitutions, comments, functions and embedded/quoted placeholders are conservatively refused. Static scripts without placeholders retain previous shell semantics.

`dangerously_allow_raw` plus `dangerously_unquoted_shell` is the explicit unsafe escape hatch. Neither implicit legacy raw nor unknown filters are admitted. Typed `ExpandedCommand` retains provenance through popup/none/terminal dispatch; Debug and error paths omit data. Terminal and remote edges project once through shell quoting. A program itself may interpret its arguments (for example eval); trusted configuration remains executable authority.

Invalid command configuration produces field-indexed diagnostics and runtime refusal without discarding unrelated configuration. Strict config writes reject invalid command definitions. Template/argv/output bounds and NUL checks reject before execution.

No arbitrary OS process cancellation or remote ownership guarantee is claimed here; THE-378 remains the command lifecycle issue.
