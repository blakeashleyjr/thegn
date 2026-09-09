# Tasks — file-manager provider seam (THE-14)

- [x] Add the object-safe `FileManager` seam, capability/error/spawn/control
      vocabulary, and pure factory/resolution policy in core.
- [x] Add `[drawer] kind`: implemented yazi/custom, reserved lf/broot, with
      backward-compatible command resolution and strict diagnostics.
- [x] Move yazi-specific configuration, theme, plugins, and OSC control behind
      its provider; implement the capability-free custom provider.
- [x] Make drawer spawn, containment, pooling/prewarm, and control scanning
      manager-agnostic and capability-gated.
- [x] Add provider probes to doctor and conformance coverage without starting a
      manager.
- [x] Document the kind, compatibility rule, capabilities, and drawer behavior.
- [x] Cover factory/kind/config, yazi/custom behavior, vendor isolation,
      containment, probe/conformance, and help/config gates.
- [x] Land the reviewed seam in `10e71c4d` and strictly validate this change.

## Validation boundary

The former unchecked list reflected task bookkeeping, not missing code. No new
historical full-CI claim is made by this reconciliation.
