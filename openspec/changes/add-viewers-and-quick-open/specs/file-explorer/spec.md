# File Explorer

## ADDED Requirements

### Requirement: Document viewers in the preview pane

The preview pane SHALL render Mermaid diagrams, PDF documents, CSV files as a
table, and Jupyter notebooks as additional content-type routes on the existing
preview seam, reusing the tree-sitter text route and the kitty / iTerm / sixel
graphics route, and MUST fall back to a text representation when the terminal
lacks graphics support. All document parsing and rasterization runs off the
event loop, and these viewers depend on no AI/agent layer.

#### Scenario: CSV renders as a table

- **WHEN** the user previews a `.csv` file
- **THEN** the preview pane shows the data as a bounded, scrollable table of rows
  and columns

#### Scenario: Mermaid renders via the graphics path on a capable terminal

- **WHEN** the user previews a Mermaid document on a terminal that supports
  kitty / iTerm / sixel graphics
- **THEN** the rendered diagram is shown through the graphics preview path

#### Scenario: PDF falls back to text without graphics support

- **WHEN** the user previews a PDF on a terminal with no graphics support
- **THEN** the preview pane shows an extracted-text representation instead of
  failing

#### Scenario: Jupyter notebook renders cells in order

- **WHEN** the user previews a `.ipynb` notebook
- **THEN** its cells render in order with code cells highlighted, markdown cells
  shown as text, and image outputs shown via the graphics path

### Requirement: Drag-drop affordances via the TUI mouse seam

The file tree SHALL support dragging a file via the in-process TUI mouse seam so
that releasing over a terminal pane inserts the file's path into that pane's
input, and releasing over a markdown preview or editor inserts a markdown link;
this is limited to in-process panes and requires no AI/agent layer.

#### Scenario: Drag a file onto a terminal pane

- **WHEN** the user drags a file from the tree and releases over a terminal pane
- **THEN** the file's path is inserted into that pane's input

#### Scenario: Drag a file onto a markdown surface

- **WHEN** the user drags a file from the tree and releases over a markdown
  preview or editor
- **THEN** a markdown link to the file is inserted
