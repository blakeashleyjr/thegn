# Support

thegn is a small, fast-moving alpha project. Here is where to go:

- **Getting started / usage:** the [README](README.md) tour, the in-app help
  (press `F1`), [`docs/help/getting-started.md`](docs/help/getting-started.md),
  and the pages under [`docs/help/`](docs/help/).
- **Known issues:** check [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) first.
- **Configuration:** every key is documented in
  [`config/config.toml.example`](config/config.toml.example).
- **Bugs and feature requests:** open a
  [GitHub issue](https://github.com/blakeashleyjr/thegn/issues) using one of the
  templates.

## Before you file

- Run `thegn doctor` and include its output.
- For anything beyond a typo, run `thegn doctor bundle --out thegn-bundle.tar.gz`.
  It writes a redacted archive locally and prints a manifest of what it contains.
- Open the archive and read the manifest before you attach it. Redaction is
  best-effort; remove anything you do not want public (private repository names,
  paths, tokens). Never attach it to a public issue if it contains anything
  sensitive; describe the problem in words and offer the details privately instead.

## Questions and ideas

Use [GitHub Discussions](https://github.com/blakeashleyjr/thegn/discussions) if
enabled; otherwise open an issue with the feature request or bug report template
and say it is a question.

Only the latest alpha release is supported. There is no SLA; this is a personal
project shared in the open.

**Security issues:** do not use public channels — see [SECURITY.md](SECURITY.md).
