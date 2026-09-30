# Coordination brief — THE-709

## How this lane is run

You are one lane of a maintenance batch. A primary agent (the Lead) reviews
every plan and every diff before anything merges. Your job is this issue and
nothing else.

### Cargo — you MAY run these, and should

The stage prompt forbids Cargo. The primary relaxes that for exactly two
commands, because a report of `implementation-ready` for code that does not
compile is worthless:

    nix develop --command cargo check -p <crate> --all-targets
    nix develop --command cargo test  -p <crate> --lib <narrow filter>

Run them as often as you need. Everything else — `build`, unfiltered `test`,
`nextest --workspace`, clippy, any `just` recipe — stays the primary's.

**Attempt it; if `nix develop` fails inside the sandbox, report that verbatim
and stop.** The pipeline sandbox read-only-binds `/nix/store`, so `nix develop`
sometimes cannot create its temporary store paths. That is an environment
limitation, not your failure, and "checks not run, nix develop failed with
<error>" is a perfectly good report. What is NOT acceptable is claiming the
code compiles without having checked.

### Database safety

**Never run a thegn command that opens or migrates the real database.**
`$XDG_STATE_HOME` defaults to the user's live state and a migration from your
shell mutates the running instance's DB. Any test needing a DB must build its
own fixture in a temp dir.

### `.thegn/maintenance/` hygiene

This directory carries previous batches' records. You may add files under
`.thegn/maintenance/<ISSUE>/`. **Do not delete anything** — a deletion on this
branch deletes it from main when the lane merges.

### Ratchets that a change like this trips

`just lint` / `just test` enforce shrink-only allowlists. Expect to need an
update in the same change for: a new `section.key` in config (three ratchets —
`config.toml.example` key coverage, the env-overlay list, and the completion
slot), a new capability row (surface-gaps), a new action id (help page
`actions:` frontmatter _and_ prose), a control-API schema snapshot
(`THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema`),
platform `#[cfg]` outside `platform/`, colour/glyph literals outside the caps
chokepoints, and ignored `Result`s. Never add a ratchet entry without a written
reason.

### Report vocabulary

Exactly one of: `plan-ready`, `implementation-ready`, `source-review-clear`,
`revisions-needed`. Never PASS/APPROVED.

## Scope for THE-709

**In scope:** a lint-time check that every `uses:` ref in
`.github/workflows/*.yml` (and any composite action under `.github/actions/`)
is a 40-hex commit SHA, wired into `just lint` alongside the existing ratchets,
plus a reasoned allowlist for local `./`-relative actions.

**Constraints the primary is setting:**

1. Follow the **existing ratchet idiom** rather than inventing one. Read
   `test/*-ratchet.txt` and whatever in the justfile checks them, and match that
   shape: a shrink-only allowlist file with a header comment explaining each
   entry's reason, and a checker that fails on anything not listed.
2. The local `./.github/actions/ci-setup` reference **cannot** be SHA-pinned —
   it resolves within the repo at the workflow's own commit, so it is already
   immutable. Allowlist `./`-prefixed refs _as a rule with a written reason_,
   not as an individual entry; an entry-per-local-action would be noise.
3. It must **fail** if someone reintroduces `actions/checkout@v4`. Prove that:
   include a test (or a self-check in the script) that feeds it a mutable ref
   and asserts a non-zero exit. A checker nobody has seen fail is not a gate.
4. Keep the comment next to each pinned SHA that says which tag it corresponds
   to — the check must not demand those comments be removed.
5. **Do not** change any workflow's behaviour, triggers, or the currently
   commented-out `push`/`pull_request` keys in `ci.yml`. Remote CI being off is
   a deliberate cost decision documented above that `on:` key.

**Out of scope:** updating any pinned SHA to a newer release, adding a
dependabot/renovate config, and anything about the workflows' content.
