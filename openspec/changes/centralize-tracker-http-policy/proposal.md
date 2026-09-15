# Centralize authenticated tracker HTTP policy

## Why

Linear, Jira, and Kaneo each construct an independent authenticated HTTP client
and collect response bodies without a shared limit. Redirects, account-origin
confusion, decompression, and a stalled multi-request mutation can therefore
replay credentials, consume unbounded memory, or exceed the intended operation
budget.

## What changes

Add one process-wide eight-permit tracker HTTP budget and one absolute 20-second
deadline per logical provider operation. Each account keeps its own parsed
scheme/host/port origin and authorization material. Clients explicitly disable
redirects and gzip, brotli, deflate, and zstd decompression, use five-second
connect/read ceilings, reject non-identity encodings, validate JSON MIME types,
and stream raw responses through a one MiB cap.

Mutation bodies are serialized through a 512 KiB bounded serde writer before
dispatch. The same operation is passed through every request in provider
sequences, including status lookup/mutation and issue refreshes. Errors retain
provider/phase/status policy fields only; credentials, query values, and
arbitrary response text are not copied into diagnostics.

Explicitly configured self-hosted HTTP origins, including LAN origins, remain
supported. Unsupported schemes, credentials, paths, queries, fragments, and
origin escapes are refused. Future cancellation is carried by dropping the
owned provider future; no detached body or decode task is introduced.

## Scope

This change covers Linear, Jira, and Kaneo authenticated REST/GraphQL calls and
source-level fake HTTP fixtures. It does not add pagination, account authority,
identifier grammar, plugin HTTP clients, or a host-wide refresh lifecycle.
