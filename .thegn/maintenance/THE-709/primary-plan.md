# THE-709 — primary greenlight

**Greenlit.** Implement the plan in `.thegn/pipeline/THE-709/maintenance-investigate/691.md`,
with the scope cuts and additions below. Where this document and the
investigation disagree, this document wins.

## The finding I verified myself, because it changes the job

The investigation's central claim is that **the issue is partially stale** —
`test/action_pin_test.py` already exists and already does the recursive scan the
issue asks for. I checked rather than took it:

- `test/action_pin_test.py` is present (6,759 bytes) with
  `test/action-pin-allowlist.txt` beside it.
- It is invoked from exactly one place: `just test-action-pins`
  (`justfile:1162-1163`).
- That target is a dependency of `test:` (`justfile:784`) and of **nothing
  else**. `ratchets:` (`justfile:636`) and `lint:` (`justfile:701`) do not
  reach it.

So the issue's "nothing enforces it" is wrong as written, and the real gap is
narrower than the issue describes: enforcement exists at **test** time, the
issue asks for it at **lint** time, and nothing proves the checker can fail.

That makes this a two-line-of-intent change, not a new ratchet. **Do not write
a second scanner.** A duplicate would be worse than the gap: two checkers drift,
and the one nobody runs is the one that goes stale.

## Approved scope

1. **Wire the existing checker into `ratchets`.** Add the invocation next to
   the other source-only policy checks in the `ratchets` recipe, so `just lint`
   reaches it through its existing `lint: ratchets` dependency.

2. **Prove it can fail.** Add a hermetic self-check that feeds a mutable ref
   (`actions/checkout@v4`) to the same validation code path and asserts a
   non-zero / rejecting result. Cover, at minimum:
   - a mutable tag ref → rejected;
   - a valid 40-hex SHA with its trailing version comment → accepted;
   - a `./` local action → accepted, **and its callee scanned** (the recursion
     is the part most likely to rot silently);
   - a pinned external ref whose version comment was removed → rejected.

   It must not touch the checked-in workflows. Build fixtures in a temp dir.

3. **Leave `test-action-pins` in `just test`.** The investigation offers to
   deduplicate the two call sites through a shared target. Don't. Running the
   check in both gates costs milliseconds, and the issue's whole complaint is
   that a check reachable from only one place is a check that can be bypassed.

## Explicitly out of scope — do not do these

- **Any change to a workflow file or `.github/actions/ci-setup/action.yml`.**
  The investigation confirms all 37 external refs are already SHA-pinned with
  version comments. Nothing needs repinning.
- **`ci.yml`'s commented-out `push`/`pull_request` triggers.** Remote CI being
  dispatch-only is a deliberate cost decision documented above that `on:` key
  (~101 billable minutes per push). Do not uncomment, "fix", or comment on it.
- **Updating any pinned SHA to a newer release.** Not this issue.
- **Adding dependabot/renovate config.** Not this issue.
- **Adding local `./` actions to `test/action-pin-allowlist.txt`.** The `LOCAL`
  rule in the checker already handles them as a _rule with a stated reason_,
  which is what the issue asked for. Entry-per-local-action would be noise and
  would make a shrink-only list grow for no reason. The allowlist stays empty.

## Answering the investigation's one open question

It asks whether to deduplicate the `test` and `lint` call sites. Answered above:
no. It reports no other blockers, and I agree there are none.

## Verification you owe me

- `python3 -B test/action_pin_test.py` passes against the real tree.
- The new self-check **fails** when pointed at a mutable ref. Show me the
  non-zero exit, not a description of it. A gate nobody has watched fail is not
  a gate — that is the entire reason this issue exists.
- `git diff` touches only `justfile` and `test/action_pin_test.py`
  (and `test/action-pin-allowlist.txt` only if you can justify it, which I do
  not expect).

No Cargo is needed for this lane at all — it is Python and a justfile recipe.
If you find yourself wanting `cargo`, stop and tell me why.
