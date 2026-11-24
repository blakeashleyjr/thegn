# Tasks — bounded argv-template tokenizer

## 1. Tokenizer and size bound

- [x] 1.1 Add the pure per-element tokenizer with typed malformed-input errors.
- [x] 1.2 Preserve shell-looking characters as literal argv content and render
      token values verbatim.
- [x] 1.3 Bound template input and rendered output at
      `MAX_TEMPLATE_ELEMENT_BYTES` (16 KiB) with `TemplateError::TooLarge`.
- [x] 1.4 Add unit fixtures for brace escapes, every error path, shell-literal
      text and both size limits.
- [ ] 1.5 Run `just ci` (includes `openspec-validate`).
