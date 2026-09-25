Primary review addendum while row 485 was active

This file is a review request, not a claim that the live worker received terminal input. Native workers run codex exec; PTY send merely echoes text and is not a reliable steering channel. Address these before approval or report them pending.

1. address_allowed uses Ipv6Addr::to_ipv4, which converts ::1 to 0.0.0.1 and breaks explicit private-network IPv6 loopback access. Use mapped-only conversion or preserve native IPv6 semantics. Test ::1 with/without opt-in; :: always denied; mapped loopback follows IPv4 policy; deprecated compatible addresses explicitly refused. Standalone proof is recorded in the primary batch ipv6-loopback-probe.txt.
2. bounded_reserve_needed must return zero when new_len <= capacity, otherwise every small chunk doubles capacity unnecessarily up to 32 MiB. Add a repeated small-chunk actual Vec regression showing capacity stays proportional to received bytes, plus spare-capacity overflow regression.
3. Current same-client DNS rebind fixture uses private=true then 10.0.0.1, an intentionally admitted address. A connection failure is not proof of resolver refusal. Use a categorically forbidden destination and assert policy resolution fails, keeping the same client and forced reconnect. Verify accepted request and resolution counts.
