# Predictive local echo for high-latency remote panes

## Why

A sprite pane round-trips every keystroke to the provider before it echoes —
measured **~320 ms RTT** to `api.sprites.dev` (the sprite VM is in a fixed,
distant fly.io region; sprites expose **no region selection** and **no UDP**, so
neither config nor mosh is available). The lag is the RTT, so it is **independent
of the transport** — the WSS exec PTY and ssh-over-WSS are equally laggy. The
only fix is what mosh does: **echo the user's keystrokes locally, immediately, and
reconcile when the server's authoritative output arrives.** superzej owns the
vt100 emulator + the pane, so it can do this transport-agnostically — it works on
the *existing* native-exec pane (no ssh, no fresh sprite, no re-provision).

## Scope (conservative first cut)

Mosh's engine does per-cell epoch validation; we start with a smaller, low-glitch
subset that covers the 90% case (typing a command at a shell prompt):

- **Predict printable keystrokes only.** On a `Key(char)` to a high-latency pane,
  append the char to a per-pane *prediction overlay* and advance a *predicted
  cursor*; render it immediately (dimmed/underlined, like mosh) at the cursor.
- **Backspace** pops the last predicted char; **Enter** flushes the overlay (the
  line is submitted — the server will redraw).
- **Reconcile by clear-on-output.** When real `PaneEvent::Output` for the pane
  arrives, **drop the whole overlay** — the server's bytes are authoritative and
  already include the echoed text (or the correct state). Since the echo lands
  ~320 ms later, the prediction shows for ~one RTT then is replaced seamlessly
  (no visible change when the prediction was right; the server corrects it when
  wrong). This avoids mosh's per-cell validation while staying safe.
- **Safety gates — predict ONLY when:**
  - the pane is **remote/high-latency** (native-exec/ssh provider pane, or a
    measured srtt over a threshold, e.g. 50 ms); local panes never predict;
  - the emulator is **not in the alternate screen** and **not in application /
    raw / bracketed-paste mode** (vim, htop, fzf, … manage their own echo —
    predicting there corrupts the display). The emulator already tracks
    alt-screen + DECCKM; expose them.
  - the cursor is on the **last row** (a prompt line), not mid-screen.
- **Display gate:** only render predictions once latency is actually high (mosh's
  "show predictions only when they help") so a fast link is untouched.

## Where it hooks (real code)

- `crates/superzej-host/src/pane.rs` (`PtyPane`): add a `Prediction` overlay
  (buffer of predicted chars + predicted cursor col, an `enabled` flag, and an
  srtt estimate). `feed()` (server output, pane.rs:427) **clears** the overlay;
  `write_input()` (439) is where keystrokes already go.
- `crates/superzej-host/src/emulator.rs`: expose `alt_screen()` / `app_cursor()` /
  cursor position from the alacritty emulator (the `PaneEmulator` trait) so the
  predictor can gate + place the overlay.
- Input path (`run.rs`, the focused-pane `Key` handling near `write_input`): on a
  printable key, also push to the pane's prediction overlay (gated) + mark dirty.
  Backspace/Enter update/flush it.
- Compose (`compose_pane` / the pane render in `run.rs`): after composing the
  emulator grid, overlay the predicted chars (dim) at the predicted cursor cells.
  This rides the existing per-pane bounded-diff (`render_plan::Incremental`), so a
  prediction is a one-pane recompose, not a full chrome frame.
- srtt: estimate per-pane round-trip by timestamping the keystroke and the next
  server output (EWMA), to drive the latency gate. Pure + testable.

## Testable vs live

- **Pure + unit-tested:** the prediction state machine (push/backspace/flush/
  clear-on-output), the srtt EWMA, and the safety-gate decision (`should_predict`
  given alt-screen/app-mode/cursor-row/srtt).
- **Needs live tuning over the 320 ms link:** the display threshold, the dim
  styling, and the alt-screen/app-mode heuristics (false predictions in TUIs are
  the main glitch risk). This is why it's a focused build with a dogfood loop,
  not a one-shot.

## Non-goals (first cut)

- No multi-line / wrapped-line prediction, no per-cell epoch validation (mosh's
  full engine) — deferred; clear-on-output covers the prompt case.
- Doesn't reduce throughput latency (paging output) — only keystroke echo, which
  is the felt lag.
- Orthogonal to ssh-over-WSS and host-parity (both still apply); ssh's value is
  now scp/sshfs/cleaner-PTY, NOT lag (it can't beat RTT).
