# Design

`thegn-svc::issue::identity` owns syntax-only admission. Built-in path and CLI
segments are bounded to 128 bytes, reject controls, path delimiters and
option-like prefixes, and a complete built-in identity is bounded to 256 bytes.
GitHub accepts positive decimal issue numbers and validated `owner/repo` under
the configured `GH_HOST` (default `github.com`); Jira accepts
`PROJECT-[1-9][0-9]{0,19}`; Kaneo ids remain bounded opaque slugs/CUIDs/UUIDs.
Plugin keys have a separate 512-byte UTF-8 envelope and may contain Unicode or
delimiters after control-character admission.

GitHub URL parsing uses the URL authority and rejects credentials, lookalike
hosts, malformed owner/repo paths, and malformed scoped ids. Provider responses
are checked before domain conversion. Jira and Kaneo path construction checks
all dynamic ids before interpolation. Control clients percent-encode the full
identity as one segment; the server receives and decodes it once. No generic
delimiter ban is applied to plugin business keys.
