# Serialize and validate FRP configuration without TOML injection

THE-326 replaces interpolated `frpc.toml` text with a typed document boundary.
The FRP share planner validates provider values before it serializes the
document, reparses the resulting text into the same strict types, and compares
the parsed document with the intended value before materialization.

The change covers generated FRP server/auth/proxy fields and keeps the
existing share provider seam. It rejects nonempty raw `extra` entries until
each field has an audited typed representation. It does not add a provider,
credential source, port discovery, or runtime networking authority.
