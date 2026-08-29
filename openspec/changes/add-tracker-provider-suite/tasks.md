# Tasks — tracker seam audit and cheap gaps

1. **Typed seam contract and conformance**

   Add `IssueCaps { comments, labels }` in a new issue module; add
   `IssueBackend::caps`; return typed `Unsupported` from optional defaults;
   implement `SeamError` for `IssueError`; declare native caps; extend the
   offline conformance table across every `IssueProviderKind::ALL`; and include
   overclaim/underclaim tests without network or subprocess I/O.

2. **Plugin and doctor integration**

   Parse the existing contribution `caps` JSON for issue providers, default
   omitted/null to false, refuse false-cap operations locally, map plugin
   `unsupported` replies to typed Unsupported, and preserve the existing
   five-op bridge. Include caps in configured native doctor reports without
   starting resident plugins or probing the network.

3. **Extension documentation**

   Add the native/plugin tracker-provider recipe, link it from the extending
   docs, and document the constraints for a future concrete CLI-backed
   provider. Do not add a generic command config key.

4. **Validation**

   Run only the scoped commands listed in the pipeline chunks. Do not run e2e,
   a full-workspace build, or `thegn` against the live state DB. If a binary
   invocation is required, set `XDG_STATE_HOME` to a new temporary directory.
