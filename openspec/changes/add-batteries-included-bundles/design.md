# Design — batteries-included editions

## Is shipping a terminal in thegn's lane? (the argument)

Three readings of "distribution with a fully featured terminal", judged:

1. **Fork/embed an emulator** (ship our own terminal binary or link an
   emulator crate as the outer surface). Rejected. It makes thegn the
   security maintainer of an escape parser, font shaper and GPU pipeline —
   an attack surface upstream emulators patch continuously and we would
   patch on our release cadence. It also collides head-on with
   `define-gui-frontend-lane`'s pin: any future graphical surface is a thin
   client of the daemon, never a second in-process render backend. And it
   abandons `terminal-compat`, the capability that makes thegn good in the
   terminal the user already loves.
2. **A GUI app that hosts a terminal widget**. Rejected here for the same
   reason, and explicitly owned (as a _future_, criteria-gated lane) by
   `define-gui-frontend-lane` — a batteries edition must not preempt it.
3. **Compose a pinned upstream emulator + font + tools around the existing
   binary**. Accepted. This is packaging: upstream maintains the emulator
   and ships its CVE fixes; thegn maintains a config file (the bundled
   profile) and a wrapper. It is the same lane `macos-app-launcher` and
   `nix/package.nix` already occupy.

The spec'd rule: batteries editions compose; they never embed. The only
thegn-owned artifacts are the profiles (`config/alacritty.toml`,
`config/ghostty.config`), the wrappers, and the pins.

## What exists today (inventory the change builds on)

| Piece                                  | State                                                                                                                                      |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| Bundled profiles (alacritty + ghostty) | shipped; both name FiraCode Nerd Font; option-as-alt + TERM handled                                                                        |
| `tg --standalone` + Linux `.desktop`   | shipped; **errors if alacritty absent**; font picker patches the profile                                                                   |
| macOS `thegn.app` generator            | shipped (`macos-app-launcher`); resolves any of 5 emulators; only alacritty gets the bundled profile; Terminal.app fallback loses fidelity |
| Runtime deps                           | guaranteed on nix only (`package.nix` wrap); tarball installs rely on `thegn doctor`                                                       |
| The font itself                        | **never installed by anything**                                                                                                            |

The delta is small and honest: provision the emulator and the font, prefer
the profiled emulator everywhere, and promote "full fidelity" from a hope to
a contract.

## The product, per platform

| Platform                                       | Batteries product                                                                                                                        | Verdict                                                                                                              |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| **Linux (nix)**                                | `.#batteries`: pinned alacritty + Nerd Font (fontconfig-scoped) + bundled profile + wrapped thegn                                        | **now** — the reference edition; zero recurring cost (flake.lock)                                                    |
| **macOS**                                      | install-time provisioning (`--batteries`): emulator + font via nix-darwin or brew casks; locally generated `.app` on the bundled profile | **now** — keeps the Gatekeeper/no-notarization stance intact                                                         |
| **Linux (non-nix)**                            | `install.sh --batteries`: provision via the distro's manager where recognized, else verify-and-instruct; `.desktop` unchanged            | **now**, best-effort tier — "verified or precisely reported", never silent                                           |
| **Windows**                                    | Windows Terminal is stock on Win11; shape = a WT profile fragment + winget dependency story                                              | staged behind `add-windows-ci-distribution`'s msvc leg                                                               |
| **flatpak / AppImage / `nix bundle` portable** | a terminal-in-a-box for non-nix users                                                                                                    | deferred — GPU/GL driver portability (the nixGL class of failure) is the recorded entry criterion; revisit on demand |

## Emulator and font choice

**Default emulator: alacritty.** Reasons: it is already the standalone
default (`tg --standalone`, the `.desktop` entry); the in-app font picker
integration exists only for it (`THEGN_ALACRITTY_CONFIG`); it is packaged in
nixpkgs for both linux and darwin; the bundled profile exists and sets
option-as-alt and TERM correctly; undercurl is supported. Ghostty stays the
top _preference_ when the user already has it (the macOS launcher's existing
candidate order is untouched), and `config/ghostty.config` remains shipped —
but ghostty's darwin packaging story in nixpkgs is not dependable enough to
pin as the reference edition.

**Font: FiraCode Nerd Font**, because both shipped profiles already name it
(one font family, one source of truth). On nix it is scoped to the batteries
launch via a generated fontconfig file (`FONTCONFIG_FILE` in the wrapper —
system fonts included, the pinned font dir appended) rather than installed
user-wide; on macOS it is the brew cask / nix-darwin font package. A
batteries launch with the font missing is a provisioning failure to report,
never a silent fallback.

## Mechanism

- **`.#batteries` (flake output, `nix/batteries.nix`)**: a wrapper package
  whose binary launches the pinned alacritty with
  `--config-file <bundled profile>` running the wrapped `thegn` (which
  already carries the runtime tools on PATH), exporting
  `THEGN_ALACRITTY_CONFIG` so the font picker patches a writable copy of the
  profile (first-launch copy into `$XDG_CONFIG_HOME/thegn/`, since the store
  path is read-only — the same file the picker edits thereafter). Also the
  `.desktop`/app-launcher entry points at this wrapper.
- **macOS `--batteries`**: `install.sh` / `just macos-app` gain a flag that
  (a) detects an acceptable emulator; (b) if none, installs via what exists —
  brew casks (`alacritty`, `font-fira-code-nerd-font`) or a nix-darwin hint —
  with the user's confirmation; (c) generates the `.app` pinned to the
  provisioned emulator + bundled profile; (d) prints a per-item
  provisioned/present/missing summary. No brew and no nix ⇒ report the exact
  casks/packages needed and exit nonzero without half-registering a launcher.
- **Fidelity check**: the batteries wrapper environment must make
  `terminal-compat` detection resolve truecolor + full glyphs + undercurl
  (alacritty profile sets TERM/COLORTERM appropriately). `thegn doctor`
  already reports caps and tool probes; the rehearsal step records a doctor
  run from a batteries launch as the verification artifact (the
  verified-before-advertised rule from `add-package-manager-releases`
  applies to the README's batteries rows).

## Rejected alternatives

- **Embedding/forking an emulator** — argued above; also doubles binary size
  and contradicts A 5 (single-binary product).
- **Downloadable macOS `.app`/`.dmg` with a terminal inside** — a downloaded
  bundle carries quarantine; `macos-app-launcher` requires any distributed
  bundle be signed + notarized, and RELEASING.md deliberately declines that
  cost. Install-time generation keeps the guarantee without the key custody.
- **Making the batteries wrapper the default install** — most users already
  have a terminal they prefer; thegn's core promise is being excellent
  _inside it_ (`terminal-compat`). Batteries is an edition, not the product.
- **Ghostty as the reference pin** — better terminal, undependable darwin
  packaging in nixpkgs; revisit when that changes (open question below).

## Security

- **No new credentials, no signing keys.** The nix edition is pinned by
  `flake.lock`; supply chain is nixpkgs' (same trust as the existing
  package). macOS provisioning shells out to brew only with explicit user
  confirmation, installing named official casks — the installer never
  downloads binaries itself.
- **Emulator attack surface stays upstream's**: CVE response is a pin bump
  (flake.lock / cask update), not a thegn release; the composition rule is
  what keeps it that way.
- **Fonts are parsed data** (a historical exploit vector): both sources are
  the platform's official packaging of the Nerd Fonts release, pinned; on
  nix the font is fontconfig-scoped to the batteries launch rather than
  installed system-wide, shrinking exposure.
- **Blast radius**: installers write only user-scoped paths
  (`~/Applications`, `~/.local/{bin,share}`, `$XDG_CONFIG_HOME/thegn`);
  nothing touches the daemon, control plane, sandbox policy, or config
  schema. No raw tokens anywhere — there are no tokens in this change.
- **Gatekeeper honesty**: reaffirming locally-generated-only bundles means
  users are never trained to bypass quarantine prompts for thegn.

## Loop / render / state notes (per design rules)

- Render damage channels and event-loop wake paths: **untouched** — no Rust
  changes at all.
- SQLite schema: **unchanged**.
- New interactive surfaces / help contexts: **none** — no new action ids, so
  the help ratchets are unaffected; only install prose changes.

## Open questions

- **Font-picker parity for ghostty**: the picker patches only the alacritty
  profile today. If a batteries user lands on ghostty (macOS preference
  order), "Switch font" does nothing. A `THEGN_GHOSTTY_CONFIG` counterpart
  is a small host change — deliberately out of scope here (packaging-only);
  record and decide with the theming group.
- **Ghostty as reference pin** when its nixpkgs darwin packaging stabilizes.
- Should the HM / nix-darwin modules grow a `batteries = true` option that
  swaps the launcher entry to the batteries wrapper? Cheap, but defer until
  someone asks.
- flatpak on demand: a flatpak'd terminal+thegn is a normal shape
  (GL handled by the runtime) and would serve non-nix Linux better than
  AppImage — entry criterion stays "a user actually asks".
