# Establish the public Jobs v0 contract

## Why

A daemon and a hosted executor must agree on which input ran and which worker may publish its outputs. Portable fixtures let each implementation verify those decisions before it can dispatch real work.

## What changes

- Add independent Rust libraries for JobSpec and JobResult, SHA-256 directory objects, lease transitions and guest protocol framing.
- Specify strict submission parsing, single-offering capability matching, canonical directory bytes and the exact authority needed to publish or replay a commit.
- Supply portable YAML, JSON and protobuf vectors for independent consumers. Use those same vectors when integrating the daemon queue and the hosted provider.

## Impact

Roadmap BC 764 owns this work. The first increment exercises data and transitions entirely in process. Daemon admission, SQLite reservations, hosted placement and the remaining guest exchanges build on this contract. [Architecture RFC](../../../docs/rfcs/jobs.md) · [v0 contract](../../../docs/rfcs/job-spec-v0.md)
