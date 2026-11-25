# Meter AI spend in the fleet burn

## Why

The masthead `fleet` $/h meter shows only provider VM burn, so metered AI
spend (pay-per-token API usage through the model proxy) is invisible next to
it. One combined number is what an operator watches.

## What Changes

- The `fleet` widget shows VM burn plus metered AI burn (trailing hour, from
  the model proxy's audit rows). Subscription lanes are not counted.
- The zero-hide rule applies to the total.
- The fleet popup gains a metered AI block: burn, today, month to date, per
  backend/model, with estimated and unpriced provenance notes.
- The fleet poll slot also arms when the model proxy is enabled.

## Impact

`fleet` masthead widget, fleet popup, `thegn_core::proxy::stats::ai_spend`,
help pages `bars` and `ai-usage`. No new config key.
