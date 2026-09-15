# Design

`thegn-svc::issue::identity` owns syntax-only admission. Built-in path and CLI
segments are bounded to 128 bytes, reject controls, path delimiters and
option-like prefixes, and a complete built-in identity is bounded to 256 bytes.
GitHub accepts canonical positive `u64` decimal issue numbers and validated
ASCII `owner/repo` components capped at 100 bytes under
the configured `GH_HOST` (default `github.com`); Jira accepts
`PROJECT-[1-9][0-9]{0,19}`; Kaneo ids remain bounded opaque slugs/CUIDs/UUIDs.
Plugin native keys have a separate 384-byte UTF-8 budget inside a complete
512-byte control envelope and may contain Unicode or delimiters after
control-character admission.

GitHub URL parsing uses the URL authority and rejects credentials, ports,
lookalike hosts, query/fragment-bearing or non-issue paths, malformed
owner/repo paths, and malformed scoped ids; `--repo`, `-R`, and equals forms
are checked under the same grammar. The configured `GH_HOST` is read only at
the runtime boundary, while parser tests pass an explicit host. Provider responses
are checked before domain conversion, including issue number, project ids,
blocker ids, and public URL authority. Jira configured/returned project keys
are checked before JQL/path construction. Kaneo builds query pairs and URL path
segments structurally and rejects traversal, whitespace, and raw query
delimiters before any request. Control clients percent-encode the full identity
as one segment; the server receives and decodes it once. No generic
delimiter ban is applied to plugin business keys.
