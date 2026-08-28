# THE-65 — security/test/bug review verdict

Branch `tg/the-65-usage-panel`, reviewed at `8237a25f`
(after the mandated `git merge main` → `a5e3111e`, and two review-fix commits).

PASS

**Verdict: PASS** — ready for the merge queue.

## Merge with main (lead addendum, done first)

`git merge main` (97 commits behind) hit exactly one source conflict:
`crates/thegn-host/src/detail/usage_dash.rs` — main had added the
`TableSection::sel` field and a `sel: None` construction while this branch
rewrote `usage_sections` as a `usage_view` projection. Resolved in favor of
the branch's rewrite, adding main's `sel: None` to the per-account table
(`a5e3111e`). Everything else auto-merged. Note: prior lane commits are
unsigned (gpg signing times out in this harness), so the merge and fixes
follow the same `commit.gpgsign=false` convention.

Honesty note on that merge: the first pass at `d7528b9d` accidentally staged
an **incomplete** resolution (the conflict's base block survived as a stray
`|||||||` section — a non-compiling tree), which was committed before the
mistake was caught. Caught by a worktree-vs-HEAD diff before anything left
the machine: the merge was redone cleanly (`a5e3111e`, same resolution,
verified marker-free by eye and by `git grep`) and the follow-up commits
re-applied on top with their original messages; the intermediate tree is now
sound. Main also advanced again between the two merges (THE-64 landed —
no file overlaps with THE-65), so `a5e3111e` is the merge that lands, and
all scoped gates below were re-run on the final tree.

## Findings — fixed (2 commits)

### 1. `ef497d2f` — provider strings reached `Change::Text` unsanitized and unbounded

The lane's distinctive risk surface, confirmed real. Every provider-authored
string in the usage path flowed to the chrome raw:

- Claude `/api/oauth/usage` (HTTP body — remote data): `ClaudeLimit::label()`
  concatenated `group`/`kind` + scope `display_name` verbatim into
  `UsageWindow.label`, which the new view renders as `MetricRow.name`, the
  overlay/panel headings' neighbor data, and half of the `{account}#{label}`
  history key.
- Antigravity quota body (HTTP): window `label` alias and `planType`.
- Codex rollout JSONL: `plan_type`.
- Claude `.claude.json` / `.credentials.json`: email, org name, seat/rate-limit
  tiers — which feed `AccountView::facts`, `account_label` (via
  `with_identity`/`display_label`), and the statusbar badge's
  `short_label()` (clipped through `seg::take_cols`).

A `\r` in any of them is **acted on** by termwiz in `Change::Text` — it paints
at column 0 of the underlying chrome, outside every clip rect, and from the
last row scrolls the composed frame (the documented weather incident,
`846c3929`); control bytes also disagree between the width models
(`seg::cut` counts 0, `seg_width` counts 1), and an unbounded label blows out
`table_col_widths`/chip clipping. The seg.rs comment already said "text from a
network source can reach here"; the usage surfaces are exactly that path, and
this lane widened it (facts line, forecast phrases, plain-language names on
three surfaces).

Fix per the weather precedent: `safe_text`/`safe_field` at the **decode seams**
in `thegn_core::usage` (control chars dropped, 64-char cap per field, trim,
control-only/blank field ⇒ `None`), applied to all five parsers. Sanitizing at
the seam (not the view) keeps every consumer consistent — badge, panel,
overlay, alert path, and both sides of the history-key format see the same
label, so the `{account}#{label}` map keys cannot drift between the sampler
that writes them and the views that read them. Control-free labels under 64
chars (everything real providers send: `session`, `weekly`, `7d opus`, …) are
byte-identical to before; existing parse tests pin that. Three new tests cover
the hostile shapes (label with `\r`, 200-char model tail, control-only group
falling through to `kind` then `"limit"`, identity/credential fields,
antigravity/codex) and fail against the old code by construction. One
behavior improvement folded in: a group that sanitizes to empty now falls
through to `kind` rather than short-circuiting to `"limit"`.

Not affected: `home_tail` (local path, `to_string_lossy`), svc-side
`unavailable` notes (all hardcoded strings — verified each call site), Codex
window labels (hardcoded `session`/`weekly`).

### 2. `d6ccfd25` — `just lint` broke on the merged tree (not by this lane)

The main merge brought `crates/thegn-core/tests/hm_module_drift.rs`
(`048780fb`) with two clippy `nonminimal_bool` warnings that error under
`just lint`'s `--workspace --all-targets -D warnings` (verified failing before
the fix). Mechanical De-Morgan simplification; the drift test stays green.

## Findings — verified clean (no action)

- **No swallowed errors on primary paths.** The diff's `let _ =`/`.ok()` sites
  are the sanctioned best-effort set (DB history writes/prunes, already
  commented); all `.unwrap()/.expect()` additions are in `mod tests`. The
  overlay render is a pure projection; the gather→payload→channel→loop
  handoff is unchanged and payload-carried (`UsagePayload` clones history at
  snapshot; `model.usage_history` is only assigned on the loop thread), so
  there is no cross-thread custody of the rendered state and no new wake
  source or per-frame I/O — `run.rs`/`detail.rs` edits are four
  `&model.usage_cfg` parameter-threading lines.
- **Division/negative-width math.** `pct` clamps to 0..=100; `used_frac` and
  both bar branches (`viz::hbar` clamp01, `caps::bar_track` Ascii) clamp and
  saturate (`NaN`/`±inf` fractions cast to 0 — no panic, no underflow);
  `pad_to`/`bar_track` use `saturating_sub` and keep `bar+track==w`;
  `fmt_resets_in` renders negative remainders as `"now"`;
  `tone_at` guards `crit < warn`; `forecast_exhaustion` guards the slope
  denominator with `MIN_FORECAST_SPAN_SECS` and `rate <= 0`. `order` uses
  `total_cmp` (NaN-safe) and returns indices — `model.usage` is never
  reordered (badge/alert consumers keep discovery order; test-pinned).
- **Bars go through the caps chokepoint.** Both shared draw sites
  (`draw_table`, `bar_segs`) route `caps::bar_track`; the Ascii branch emits
  only `GlyphSet` fill/empty glyphs; byte-identity on Full/Basic is
  test-pinned. Ratchet tests (glyph/color/caret/platform) 13/13 green; no
  ratchet file changed.
- **Non-UTF-8.** serde JSON is UTF-8 by construction; Codex bytes go through
  `str::from_utf8` and degrade to `Unavailable`; the one `OsStr` path renders
  via `to_string_lossy`.
- **Help ratchets.** No new action id, key, or panel context (Alt-u and `r`
  are pre-existing); `docs/help/ai-usage.md` prose updated in place and still
  claims `open-usage`/`panel:usage` with the required distinctive words —
  help tests 73/73, all three ratchet allowlists untouched.
- **Frames / e2e.** Re-verified post-merge: no usage frame in any muse
  baseline, and — since the bold-dim `HeadingToned` change also touches the
  status modal — no `daemon`/`sessions`/`local tokens` string in any snapshot
  either. **No snapshot re-record is needed.** (Still true that any future
  snapshot touching these chrome pieces must be re-recorded with
  `just e2e-update`.)

## Minor notes (not fixed, not blocking)

- Pre-existing: `token_rows`/`proxy_spend_rows` pad with `format!("{:<24}")`
  (chars, not display cells) — drifts only on wide glyphs in model/route
  names, which are ASCII in practice; unchanged by this lane.
- Pre-existing cosmetic: dangling `///` doc line on `UsagePayload`; refresh
  resets the open overlay's scroll to 0.
- The overlay's fixed 88-col box clips (saturated `put_line`) rather than
  reflows on terminals narrower than 88 columns; the panel's three width
  tiers are the narrow-surface answer and are tier-tested, and the view's
  padding invariant is property-tested with wide glyphs
  (`build_pads_every_name_to_one_display_width`).

## Gates run (scoped per dev-loop policy)

`just quick thegn-core` · `just quick thegn-host` · `cargo nextest run -p
thegn-core usage::` (39, incl. 3 new) · `-p thegn-core usage_view` (15) ·
`-p thegn-core --test hm_module_drift` (2) · `-p thegn-host usage` (25) ·
`-p thegn-host sections` (75) · `-p thegn-host usage_dash status_modal` (20) ·
`-p thegn-host help` (73) · `-p thegn-host ratchet` (13) ·
`cargo clippy -p thegn-core --tests` · `cargo clippy -p thegn-host --tests` ·
`cargo clippy -p thegn-core --test hm_module_drift -- -D warnings` (red→green
proof for fix 2) · treefmt clean. All of the above re-run green on the final
tree after the merge redo. Heavy gates (`just test`/`coverage`/`ci`)
deliberately not run — `usage_view.rs` measured 428/428 lines at architect
review and is untouched by the fixes; run `just coverage` once before a PR if
wanted early.
