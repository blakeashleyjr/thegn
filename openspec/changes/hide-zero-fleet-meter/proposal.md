# Hide the zero fleet burn meter

## Why

The top-right `fleet` meter (`$/h`, provider-VM burn) is listed by default. With
VMs listed but all stopped, or before any provider usage, it reads `$0.00/h`
on every frame. A zero number is noise on the masthead: it takes columns and
says nothing.

## What Changes

- The `fleet` masthead widget auto-hides while its burn is zero, with no
  unpriced VM and no warning. Non-zero burn, unpriced `+N`, and warnings still
  show.
- New `[bars] fleet_always_show` (default `false`) keeps the zero meter on the bar.
- Render tests cover the masthead at 200, 120, 80 and 60 columns.

## Impact

`[bars]` config (`fleet_always_show`), `fleet` masthead widget, help page `bars`,
`config/config.toml.example`, ratchets (`test/env-overlay-ratchet.txt`, config
defaults coverage).
