# File Explorer — non-text preview routes

## ADDED Requirements

### Requirement: Non-text files preview through additive routes

The preview pane SHALL extend its content-type routes with: `.docx` documents
rendered as extracted text (headings and tables preserved) through the
existing text route; archives (`.zip`, `.tar` families) listed as a bounded
table of entries (name, size); and files of unknown binary type falling back
to the hex view rather than raw bytes. All extraction and listing MUST run
off the event loop with results delivered over a channel and a waker pulse,
and every route MUST degrade to a plain-text explanation when parsing fails.
These routes depend on no AI/agent layer.

#### Scenario: A docx previews as extracted text

- **WHEN** the user previews a `.docx` file
- **THEN** the preview pane shows its extracted text with headings and tables
  through the text route, parsed off the event loop

#### Scenario: An archive lists its entries

- **WHEN** the user previews a `.zip` file
- **THEN** a bounded entry listing (names and sizes) renders, and a corrupt
  archive shows a parse-failure message instead of erroring the pane

#### Scenario: Unknown binary falls back to hex

- **WHEN** the user previews a file with no recognized route and non-text
  content
- **THEN** the existing hex view renders instead of raw bytes
