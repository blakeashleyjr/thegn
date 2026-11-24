# Add the Windows sandbox helper wire protocol

## Summary

Add a serde-only crate, `thegn-sandbox-proto`, that defines the request, probe,
status and peer wire types shared by `thegn-core` and the standalone Windows
sandbox helper `thegn-sandbox-win.exe`. It also carries the bounded, scrubbed
request transport (numbered environment chunks) and the portable Windows argv
quoting both sides must agree on.

## Why

The helper must not depend on `thegn-core` — unsafe Win32 FFI and undocumented
OS exports stay out of host and core — while core must lower a sandbox policy
to a wire type the helper decodes. A dependency-light crate in the middle is the
only shape that satisfies both. `win_cmdline` lives here because a command line
built by core is parsed by the helper, so the two implementations may not drift.

## Verification boundary

Pure fixtures cover serialization, the bounded transport and the reference
parser. The CommandLineToArgvW rules are background knowledge today: Wine proof
is WS06-P6-01 and native Windows CI proof is WS06-P1-06, both of which compare
the fixtures against the real API before anything here is treated as Windows
evidence.
