# `thegn-sandbox-proto`

Portable wire types for the Windows sandbox helper. `thegn-core` lowers a
sandbox policy into these types; the standalone helper `thegn-sandbox-win.exe`
decodes them. Neither side may depend on the other — core must stay free of
Win32 FFI and the helper must not link the whole daemon — so both depend on
this crate instead.

The crate is **serde-only**: its dependencies are `serde` and `serde_json`, it
contains no `cfg` code, no spawn, no threads and no OS calls, and it is
`#![forbid(unsafe_code)]`.
That is why it is exempt from the per-crate platform-`cfg`, spawn-ownership and
thread-name ratchets (it has nothing for those scans to pin) and why it is a
C-dep-free leaf in `just check-cross`.

## `win` — request, probe and status types

- `WinSandboxRequest` (protocol version 1, `deny_unknown_fields`) carries the
  mode (`Job` or `Mxc`), the resolved absolute `.exe` argv, the working
  directory, the explicit workload environment, the Job Object limits, the MXC
  policy, the stdio shape, the optional PSEC profile name and the status path.
- `WinJobLimits` / `WinUiRestrictions`, `WinMxcPolicy` / `WinFsPolicy` /
  `WinNetPolicy`, `WinProbeReport` / `WinOsVersion` / `PsecSupport` and
  `HelperStatus` / `PeerIdentity` are the remaining wire shapes.
- `WinNetPolicy` is internally tagged by `kind` (`none`, `open`, `outbound`,
  `proxy-only`) so an unknown policy fails closed instead of being dropped.

The helper owns the policy decisions this crate only describes: it refuses a
`version` other than `WIN_PROTO_VERSION` with exit 3, and a `.bat`/`.cmd`
`argv[0]` with exit 5.

## Request transport

Requests travel as JSON in numbered environment entries `THEGN_WINSB_REQ_0..N`.
Each chunk is at most 8 KiB, the whole request is at most 1 MiB, and chunks end
only on Unicode scalar boundaries so Windows converting each value to UTF-16
never meets half a surrogate pair.

`decode_request_env` removes every request-prefixed entry — case-insensitively,
because the Windows environment is — before it validates anything, so the
remaining vector can go straight into the workload environment without leaking
the request or a later proxy URL. Errors carry a fixed string or a chunk index
only, never request content (B30).

## `win_cmdline` — argv quoting

`join` implements the MSVCRT/`CommandLineToArgvW` rules: an argument is quoted
when it is empty or holds a space, a tab or a quote; backslashes before a quote
are doubled and an embedded quote is escaped; trailing backslashes inside a
quoted argument are doubled. NUL is refused, as is a line longer than 32,766
UTF-16 units.

`split` is the matching parser and exists **only as the fixture oracle**. The
rules have not yet been checked against the real API: WS06-P6-01 compares these
fixtures with Wine's `CommandLineToArgvW` and WS06-P1-06 repeats that comparison
on Windows CI, so do not treat the in-crate round trip as Windows evidence.

## Consumers

`thegn-core` (lowering) today; the helper crate, the egress service launch and
the Windows CI probe follow. The crate is owned by WS06-P0-03.
