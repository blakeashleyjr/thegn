## Context

Rows A3 + A5 of the convergence: the remaining enum-router / hand-list seams, made honest the way the forge was.

## Goals / Non-Goals

**Goals:** CI on the sync seam shape; one provider vocabulary; one sandbox backend table; dead ssh seam gone. **Non-Goals:** changing `EnvProviderConfig.provider` to the enum type (40 string sites, no behaviour gain — the enum is the vocabulary, the string is the wire); calendar/media/remote-provider router conversion (A6).

## Decisions

- CI sync because every impl is a subprocess (the forge precedent); `CiClient` kept as a type alias so host code reads the same.
- `EnvProviderKind::of(name)` maps unknown → `Custom`, preserving today's "unknown provider = exec_command-driven" semantics; the exhaustive factory match is the coverage gate (the factory needs credentials, so `kind_coverage` can't run it).
- `BackendProfile` is a `const fn` table rather than a trait: `Backend` is `Copy + Hash`, persisted in DB rows and used as a memo key; eleven impls sharing 80% of their code would be worse. Family keys the argv builders.
- `Backend::parse("wsl")` now `None`: reserved means no runtime; doctor/onboarding callers already treat `None` as unavailable.
- Render/event-loop impact: none. No new help context.

## Risks / Trade-offs

- [Host callers of CI were wrapped in runtimes; removing them changes nothing at runtime but is a wide mechanical edit] → clippy + the CI command tests.
- [A persisted `wsl` backend row now parses as None] → it never had a runtime; rows surface as "unknown backend" rather than pretending.
