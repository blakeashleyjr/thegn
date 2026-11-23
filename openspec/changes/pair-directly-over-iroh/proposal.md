# Pair Thegn peers directly over Iroh

Add an opt-in direct Iroh transport for the existing scoped control API. A
remote peer redeems the existing one-time pairing code over an authenticated
Iroh connection and then uses the returned control token on the same API.
Direct mode requires an explicit endpoint ID and direct socket addresses; it
does not use relay servers, address lookup, the sandbox execution listener, or
implicit local administration.

Existing HTTP pairing and `host add dumbpipe:` behavior remain available.
