# Design: bounded argv-template tokenizer

## Shape

- One caller-delimited argv element in, exactly one string out. The caller
  supplies the element boundaries; the tokenizer never groups by whitespace.
- `{a.b}` is a token path whose segments match `[a-z][a-z0-9_]*`; `{{` and `}}`
  encode literal braces; any other brace is an error carrying a byte offset.
- `parse_element` yields `Element(Vec<Piece>)`, `validate` checks every token
  against the caller's `TokenSet`, and `render_element` concatenates literals
  and scalar values supplied by the caller's `TokenSource`.
- Quotes, backslashes, `$`, backticks, command-looking text and NUL stay literal
  UTF-8 content: this is not a shell lexer, so there is nothing to expand and
  nothing to execute. Process launchers may still reject a NUL later.
- Parsing and rendering each enforce a 16 KiB cap per element
  (`MAX_TEMPLATE_ELEMENT_BYTES`) with `TemplateError::TooLarge`, so both the
  configured template and the expansion of its tokens are bounded.

## Security

The one-element rule plus verbatim insertion close argument injection through
substituted values: a value containing spaces or braces cannot split an element
or forge a token. A template is never executed, and the module has no caller yet,
so there is no behaviour change on any launch path.

## Gates

- Render damage channel: none. The module renders no chrome and is not on the
  render path (`Skip`/`Panes`/`Full` are untouched).
- Event loop: none. No I/O, no spawn, no waker; every function is pure.
- SQLite schema: none. No table, no migration, no `user_version` bump.
- Help and config ratchets: none. No action, keybind, zone, config key, help
  page, capability or plugin verb is added.
- Coverage: the module lives in `thegn-core`, whose 95% line gate applies; its
  unit tests cover every parse, validate, render and error path.
