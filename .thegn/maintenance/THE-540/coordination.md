# Coordination brief — THE-540 (primary)

## Scope

`crates/thegn-svc/src/ci.rs`, `crates/thegn-host/src/cmd/ci.rs`,
`crates/thegn-host/src/daemon/service.rs` (the CI entry points only), and the
pure validators, wherever they land. The issue names THE-539, THE-322, THE-172,
THE-538 and THE-114; **none is a prerequisite and none is in scope.** THE-322 is
the same class of defect in the _tracker_ providers — do not fix it here, however
tempting the shared shape is. Report it as a finding if you see a way to share the
grammar later.

## The shape the primary expects

This is **argv and URL confusion, not shell injection**, and the issue says so.
Shell quoting is not the fix and a plan that reaches for it has misread the
defect. Two separate mechanisms:

1. **Canonical typed identifiers at ingress**, revalidated when read back from
   provider JSON or a cache — the issue is explicit that cached and remote values
   must use the same validator, which is the part most likely to be skipped.
2. **Structured construction**: a query builder that encodes each component for
   its role, and an option terminator (`--`) with options placed before it where
   the CLI supports one. The terminator is a belt, not the braces — validation
   stays mandatory.

The `run ID = --help` case is the one to keep in mind: it makes a mutation
_report success_ without mutating. That is why "no mutation can report success
because gh printed help" is a separate acceptance criterion from validation.

## Where the pure logic goes

Grammars and the command/URL builders should be **pure functions with unit
tests** — given inputs, assert exact argv and exact URL. That is what makes the
"zero requests for rejected inputs" criterion checkable without a network.

If pure validators land in `thegn-core`, remember it is **substrate-free** (no
HTTP types, no provider SDK) and gated at **95% lines**.

## Constraints

- **Vendor CLIs (`gh`, `glab`) only inside their implementation files.** There is
  a shrink-only ratchet on `gh` calls outside the forge implementation; a new call
  site elsewhere fails the build.
- **No real provider mutation in tests**, and no network. Assert on constructed
  argv/URL, never on a live call.
- Provider IDs today accept "arbitrary strings or any JSON number". A bounded
  integer grammar must reject signs, decimal and exponent forms, and excessive
  length — enumerate what you reject and test each, because this is precisely the
  list a plausible-looking regex gets wrong.
- Rejections must name the specific observation, not the category.

## A question for the primary, if you reach it

If honouring "IDs read from provider JSON use the same validator" would make
thegn reject data a provider genuinely returns today, **stop and report it**
rather than loosening the grammar. That is a decision about compatibility, and it
is the primary's to make.

## Deliverable

A plan the primary reviews and greenlights before any implementation. No
production edits in this stage.
