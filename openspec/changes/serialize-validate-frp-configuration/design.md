# Design

`plan_frp` validates the server endpoint as an IP address or bounded DNS name,
the worktree/subdomain labels, nonzero local ports, and nonzero TCP/UDP remote
ports. HTTP(S) uses the existing vhost/subdomain URL derivation. TCP/UDP keeps
the existing fixed-address contract, so `remote_port = 0` is refused before
document construction; no server-assigned-port discovery is introduced.

The generated document is represented by private `Serialize + Deserialize`
structs for the root, auth table, and proxy array. Every struct uses
`deny_unknown_fields`; optional auth/subdomain/remote-port fields are omitted
with typed `Option`s. `toml::to_string` is followed immediately by
`toml::from_str::<FrpDocument>` and an equality comparison. Tokens therefore
retain serializer-supported UTF-8/control content in the file while
`SharePlanFile` debug output continues to redact file contents.

The application policy permits existing ASCII DNS labels up to 63 bytes and
rejects uppercase, separators, controls, empty labels, and overlong derived
subdomains. IPv6 is stored unbracketed in `serverAddr` and bracketed only when
forming the fixed URL authority. `server_port = 0` remains an FRP default.
Nonempty `extra` is a terminal refusal because no safe allowlist is audited.
