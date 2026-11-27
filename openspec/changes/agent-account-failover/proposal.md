# Agent account failover

Agent roles may declare ordered harness, account and model backends. `agent run`
uses per-account usage snapshots to select an available backend, with an
explicit backend pin for operators. Credentials remain in their harness-owned
credential homes.

This change covers headroom-based selection and explicit backend launch for the
CLI. Automatic retry after provider limit errors and pipeline/lane attribution
remain follow-up work.
