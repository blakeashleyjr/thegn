# Verify native forge connectivity evidence

## Why

The landed THE-622 typed Octocrab repair has native tests for error envelopes,
status classes and fallback. Those tests inspect the private circuit rather
than the actual global connectivity holder, leaving a global-report regression
undetected despite correct local counters.

## What changes

Add an isolated exact test child that exercises actual SDK responses and the
real fallback ladder while checking global connectivity and failure counters.
Reuse existing THE-611 child custody through shared test-only source. Production
networking and Cargo dependencies remain unchanged. The helper is safe under
ordinary parallel libtest without a shipping reset API or real provider calls.

## Impact

Owned only by THE-622 (Alpha Reliability & State Compatibility). The earlier
harden-live-diagnostics-and-forge delivery remains historical and unchanged.
Native, counterfactual, review and local landing gates remain required.
