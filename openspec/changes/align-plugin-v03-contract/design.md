# Design — plugin v0.3 contract alignment

## Canonical support facts

A core-owned table records, for every `ExtensionPoint` and `HostVerb`, its wire
version, host support state, required capability/scope, accepted plugin mode,
and owning implementation/change where reserved. Loader negotiation,
`plugin check`, tests, and generated documentation consume or verify this table.

Support states distinguish:

- **wired:** accepted and served by the general host runtime;
- **separate adapter:** usable only through a named subsystem contract;
- **reserved:** decodes on the wire but negotiation rejects it;
- **unknown:** preserved/diagnosed for forward compatibility.

## v0.3 compatibility

All v0.3 wire additions default. A v0.2 single-line `View.spans` retains its
serialization/render behavior; newer rows/slots do not make it invalid. Hosts
accept compatible lower minor versions and reject unsupported major/newer
requirements with a stable diagnostic.

## Scope truth

The scope lattice is documented and tested identically across manifests,
schema, help, and host-call enforcement: `read`, `write`, `git`, and `exec` are
independent grants; `admin` implies all. Extension surface capabilities are not
subprocess authority, and declaring a contribution never grants `exec`.

## Reserved surfaces

`PanelSection` stays reserved pending THE-108 runtime negotiation/render/cache/
placement. `SidebarTab` and `Theme`, and any future key-zone vocabulary, stay
unsupported pending THE-107. The matrix must make those facts impossible to
mistake for runtime support.
