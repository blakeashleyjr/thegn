# Design

THE-50 is an audit and seam-hardening change. The authoritative design is
`.thegn/pipeline/THE-50/architect/design.md`.

The live issue seam is `thegn_svc::issue::IssueBackend`: an object-safe
`BoxFuture` trait with five required operations, three optional comment/label
operations, an account fan-out router, and a factory for Linear, GitHub, Jira,
and Kaneo. Kaneo also has board/project/move methods outside the seam through
`as_kaneo`; this change records that as a follow-up rather than expanding the
partial model.

The implementation adds a small `IssueCaps` module, typed
`IssueError::Unsupported`, shared seam classification, honest native caps,
offline conformance over every `IssueProviderKind::ALL`, and manifest-declared
plugin caps with local false-cap refusal. Configured issue probe rows serialize
the caps; doctor remains offline and does not start plugins.

The second chunk adds the native/plugin/CLI-provider authoring recipe. A
concrete CLI provider keeps its binary invocation inside its implementation
file, uses explicit argv and bounded execution, and still needs normal
factory/probe/test/config work. No generic command escape hatch is introduced.

Notion, Plane, Kaneo generic tiers, plugin doctor inventory, concrete CLI
adapters, and SDD skills are listed as follow-ups in the architect design.
