# Tasks — configurable notification sound effects

## 1. Pure core policy — complete in chunk 1

- [x] 1.1 Add `SoundRef` parsing for bell aliases, `off`/`none`,
      `builtin:bell`, `pack:<name>`, and absolute/tilde file paths. Reject
      relative paths, bare pack names, and command strings for `per_kind`.
- [x] 1.2 Resolve sound after the existing mute, route, DND, focus, and
      priority gates with rule → per-kind → legacy priority command → mode
      precedence. Preserve the terminal-bell default and command compatibility.
- [x] 1.3 Validate volume, pack syntax, kind names, per-kind references, and
      legacy chime-file syntax against `NotificationKind::ALL`.
- [x] 1.4 Add pure tests for aliases/rejection, precedence, gates, defaults,
      volume bounds, malformed kinds, and trusted repository overlays.

## 2. Host provider and playback — complete in chunk 2

- [x] 2.1 Add a platform-owned synchronous provider seam with fixed argv,
      format/volume capabilities, and `ProbeReport` output.
- [x] 2.2 Build immutable pack/file snapshots off-loop and resolve references
      without per-event filesystem access.
- [x] 2.3 Add the bounded `notify-sound` utility worker, `try_send` drop
      behavior, provider/file fallbacks, and the existing coalesced bell latch.
- [x] 2.4 Route all eligible host producers through `NotifyState`; remove
      duplicate direct sound emissions and preserve record-first ordering.
- [x] 2.5 Observe live attention edges during hydration with startup baseline,
      changed-session cues, and clear/remove handling.
- [x] 2.6 Add sound provider, pack, capability, and fallback data to doctor;
      do not add a control, database, command, or capability surface.

## 3. Configuration reference and openspec — complete in chunk 3

- [x] 3.1 Document every sound key, accepted value, default, trust boundary,
      provider capability, and terminal-bell fallback in the config example.
- [x] 3.2 Document per-kind references, event catalog names, gates, pack/file
      behavior, doctor diagnostics, and the absence of a sound control action
      in the notifications help page.
- [x] 3.3 Reconcile this proposal, design, tasks, and notifications delta
      with the compiled APIs: bell default, no synthesized family, explicit
      `pack:<name>`, fixed-argv platform provider, bounded queue, live edge,
      trusted overlays, and no DB/control/capability additions.

## 4. Scoped verification

- [x] 4.1 Run the targeted core/host/svc tests and quick checks required by the
      implementation loop, plus the relevant ratchet checks.
- [ ] 4.2 Full-workspace CI, coverage, and e2e remain deferred to the normal
      pre-PR gate; they are explicitly outside this issue's implementation
      loop.
