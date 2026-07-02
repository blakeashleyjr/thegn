# Tasks

## 1. Structured diff projection (superzej-core / superzej-svc)

- [ ] 1.1 A `review_structure` projection over `diff_sbs::parse_unified()` output:
      files + hunk headers + line ranges, with `include_patch: bool` (default off)
      — **unit tests**: structure-only is compact, `include_patch` adds text, empty
      diff yields empty structure.
- [ ] 1.2 Advertise `review_structure` + steering/comment verbs on the house-tool
      surface (`mcp/router.rs`, `HouseGit`/`HouseForge`).

## 2. Steering channel (superzej-host)

- [ ] 2.1 Map review verbs (`review_open`, `review_focus_file`, `review_goto_hunk`)
      from the ACP/MCP inbound path to the existing `PanelMsg` intents and inject
      into the event loop (channel + `TerminalWaker`) — **render test**: a steering
      verb is a chrome panel repaint, not a pane recompose.

## 3. Two-way comments (superzej-host / superzej-svc)

- [ ] 3.1 `review_comment(path, line, body)`: post through the existing forge path
      (bouncer approval applies), reflect into `PanelData.threads` on next hydrate.
- [ ] 3.2 `review_replies_since`: read human replies/annotations on review threads
      back to the agent as its next turn.

## 4. Docs + validate

- [ ] 4.1 Document the review verbs, the structured projection (patch opt-in), and
      the comment round-trip in the agent/panel doc section.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
