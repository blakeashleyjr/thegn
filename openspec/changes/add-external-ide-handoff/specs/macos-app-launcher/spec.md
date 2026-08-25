# macos-app-launcher Specification (delta)

## ADDED Requirements

### Requirement: Launcher artifacts register the thegn URL scheme

The platform launcher artifacts SHALL register thegn as the handler for the
`thegn://` URL scheme: the freedesktop `.desktop` entry declares
`MimeType=x-scheme-handler/thegn;` with a `%u`-accepting `Exec` line, and the
generated macOS bundle's `Info.plist` declares the scheme via
`CFBundleURLTypes`. Platforms with neither launcher registry get no scheme
registration and the installer SHALL NOT claim otherwise. The registered
command SHALL dispatch through `thegn url`, which handles only `thegn://open`
(focus/reveal) and `thegn://pair` (start the interactive pairing flow) and
exits non-zero on anything else.

#### Scenario: Linux registration

- **WHEN** `install.sh` writes the `.desktop` entry on a freedesktop host
- **THEN** the entry declares `x-scheme-handler/thegn` and passes the URL
  argument through to the binary

#### Scenario: macOS registration

- **WHEN** `make-app.sh` generates the app bundle
- **THEN** the `Info.plist` contains a `CFBundleURLTypes` entry for the
  `thegn` scheme

#### Scenario: Dry run names the scheme artifacts

- **WHEN** `install.sh --dry-run` runs on a platform with a launcher registry
- **THEN** its output includes the scheme registration for that platform and
  no files change
