# Tasks — per-event notification sounds

## 1. Core resolution (pure)

- [ ] 1.1 One `SoundSpec` parse (`bell` | `off` | path | pack name | command)
      shared by rule actions, `per_kind`, and `per_priority`; unit tests.
- [ ] 1.2 `notification_route.rs`: resolution order rule→per_kind→
      per_priority→mode-default, gates applied after; exhaustive table tests
      to the 95% gate (every order pair, gate interaction, DND).
- [ ] 1.3 `config_notifications.rs`: `per_kind` table, `pack`, `volume`
      (clamped 0.0..=1.0); validation — unknown kind did-you-mean, missing/
      empty pack dir warning, unmatched pack filename warning, OGG-on-Windows
      portability warning.
- [ ] 1.4 Document the new keys in `config/config.toml.example`
      (`per_kind`, `pack` layout, `volume` best-effort caveat).

## 2. Playback (host)

- [ ] 2.1 `chime.rs`: pack scan at load/reload → kind→path map (per-event
      cost stays a lookup); resolve spec → file; fall-through chain
      per_kind → pack → default.<ext> → bundled → bell.
- [ ] 2.2 Volume flags in the player command builder (`paplay --volume`,
      `pw-play --volume`, `afplay -v`, PowerShell) — runtime detection,
      no new `#[cfg]` (platform ratchet clean); shell-quoting unchanged.
- [ ] 2.3 Synthesized bundle family: distinct alert/notice tones, generated
      at first use under the state dir (still no repo audio asset).
- [ ] 2.4 Confirm zero-default-overhead: unconfigured tables short-circuit;
      no audio init in-process; sound path stays off-loop (bell latch +
      detached subprocess) — no event-loop or render-plan change.

## 3. Diagnostics + docs

- [ ] 3.1 `cmd/doctor.rs`: report resolved player, volume support, pack dir
      resolution (found/missing/empty, mapped-kind count).
- [ ] 3.2 Update the help page that claims the notification surface with
      per-kind/pack/volume prose (help-prose ratchet identifies the page);
      note the trust boundary for command specs (see
      `add-config-trust-resolution`).

## 4. Wrap-up

- [ ] 4.1 Smoke: a fake `paplay` on PATH in the hermetic smoke env asserts
      the argv (file + volume flag) without playing audio.
- [ ] 4.2 Run `just ci` once (includes openspec validate).
