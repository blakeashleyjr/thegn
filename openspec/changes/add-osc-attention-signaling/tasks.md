# Tasks

## 1. Core parse + event (thegn-core)

- [ ] 1.1 `attention.rs`: `AttentionSignal`, `parse_osc(params) -> Option<AttentionSignal>`
      for `OSC 9` and `OSC 777;notify` — **unit tests**: well-formed `9`, well-formed
      `777;notify;title;body`, missing body, non-`notify` `777` sub-command,
      oversized payload truncation, non-UTF-8 lossy decode.
- [ ] 1.2 Extend the `activity` state machine with an `AttentionRequested`
      transition that latches the waiting/needs-attention state as **sticky**
      (reusing `RESUME_GRACE_SECS`) until resume/focus — **unit tests**: signal
      sets the dot, a CPU blip does not clear it, resume/focus clears it.

## 2. Host emulator + event routing (thegn-host)

- [ ] 2.1 Forward OSC params from the `PaneEmulator` seam to `attention::parse_osc`;
      on `Some`, emit `Event::Attention { worktree, pane, title, body }` on the
      EventBus (send on mpsc + pulse `TerminalWaker`, per the event-loop invariant).
- [ ] 2.2 Wire `Event::Attention` into the existing notification derivation +
      sidebar-badge + activity-dot consumers (chrome `dirty` repaint only) —
      **render test**: an attention event marks chrome dirty, not a pane recompose.

## 3. CLI verb (thegn-host)

- [ ] 3.1 `thegn notify [--title T] [--worktree PATH | --pane ID] <body>` in
      `src/cmd/`: resolve target from flags or `$THEGN_WORKTREE`/`$THEGN_PANE`,
      raise the same `AttentionSignal` over the control path; non-zero + clear
      message when no host is live.

## 3b. Attention ordering (thegn-host)

- [ ] 3b.1 Add `SortMode::Attention` to `sidebar.rs` and an attention comparator
      in `sort_groups()`: rank `urgent > waiting > error > idle-ready > running`
      with a longest-waiting-first tie-break — **unit tests**: waiting outranks
      running, urgent outranks all, longest-waiting breaks ties, opt-in default
      leaves order unchanged.
- [ ] 3b.2 Carry an `urgent` flag on `SidebarRow` (from a `urgent_flags` map on
      `SidebarStatus`), set when `Event::Attention` carries an urgent marker and
      cleared on resume/focus — **render test**: a sort/flag change is a chrome
      repaint, not a pane recompose.

## 4. Docs + validate

- [ ] 4.1 Document `OSC 9` / `OSC 777;notify` recognition, the `thegn notify`
      verb, and the opt-in attention sort in `config/config.toml.example` (or the
      CLI/notifications doc section).
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
